# salsa-spatial bitstream specification

This document defines the salsa-spatial configuration-version-4 bitstream,
profile 1. It specifies the file structure, field encodings, parameter state,
and object reconstruction process implemented in this repository.

Sections 1 through 12 are **normative**. Annexes A and B are **informative**.

This is a repository format specification, not a registered Opus extension
standard. Annex A identifies implementation limits and places where the
current reader accepts more than the valid syntax defined here.

## Contents

- [1. Scope and conformance](#1-scope-and-conformance)
- [2. Notation and fixed geometry](#2-notation-and-fixed-geometry)
- [3. Ogg Opus transport and channel binding](#3-ogg-opus-transport-and-channel-binding)
- [4. Private extension, fragments, and records](#4-private-extension-fragments-and-records)
- [5. Configuration record](#5-configuration-record)
- [6. Tile record](#6-tile-record)
- [7. Entropy coding](#7-entropy-coding)
- [8. Parameter tile syntax](#8-parameter-tile-syntax)
- [9. Independent checkpoint syntax](#9-independent-checkpoint-syntax)
- [10. Reconstruction correction syntax](#10-reconstruction-correction-syntax)
- [11. Reconstruction semantics](#11-reconstruction-semantics)
- [12. State transitions and error handling](#12-state-transitions-and-error-handling)
- [Annex A. Reference implementation](#annex-a-reference-implementation-informative)
- [Annex B. Encoding examples](#annex-b-encoding-examples-informative)

## 1. Scope and conformance

### 1.1 Requirements

**MUST** and **MUST NOT** state requirements for conformance to this document.
**SHOULD** states a recommendation. **MAY** permits a choice. These words apply
only to normative sections.

A conforming writer MUST produce the syntax and signal meanings defined
below. A conforming enhanced decoder MUST reconstruct the same integer
parameter state and use the reconstruction semantics in Section 11. The
floating-point PCM output need not be bit-identical across FFT, Opus, or
matrix implementations.

An ordinary Ogg Opus decoder can play the public fallback channels without
interpreting Salsa records. That behavior does not constitute enhanced
decoder conformance. A decoder MAY omit the optional matrix correction, but
MUST identify that mode as baseline reconstruction rather than full enhanced
reconstruction.

### 1.2 Version domains

The version fields belong to separate layers:

| Layer | Field or identifier | Defined value |
|---|---|---:|
| Ogg Opus identification | `OpusHead.version` | 1 |
| Experimental Opus extension | Extension ID | 120 |
| Salsa extension wrapper | `SALS.version` | 1 |
| Fragment and typed-record headers | `record_version` | 1 |
| Configuration and main parameter syntax | `SCFG.format_version` | 4 |
| Profile selector | `SCFG.profile` | 1 |
| Core panner algorithm | Algorithm tag | 1 |
| Core panner algorithm version | `algorithm_version` | 1 or 2 |
| Optional correction payload | `correction_version` | 4 |

The main parameter payload has no separate version byte. Configuration
version 4 selects its syntax. A checkpoint also has no standalone version
field; its enclosing record and configuration select its interpretation.
No backward decoding rule for configuration versions 1 through 3 is defined.

### 1.3 Supported signal profile

The profile carries up to 128 ordered non-LFE objects, one separate LFE
signal, five folded ground observations, two auxiliary height observations,
and a shared silence signal. Object count is fixed within a configuration.
Object index is the zero-based position in the ordered scene table. Activity
changes MUST NOT remove or reorder table entries.

Descriptor tables have syntax for additional models, auxiliary groups, and
reconstruction stages. Section 5 defines those fields so a reader can parse
them. Profile 1 enhanced reconstruction is defined for the seven-observation
stage specified in Section 5.5. A syntactically valid descriptor does not imply
that its algorithm or stage can be executed.

The legacy five-substream API layout is not the Ogg file profile defined here.

### 1.4 External formats

Ogg page framing and checksums use RFC 3533. Opus elementary packets and
self-delimited substream framing use RFC 6716, including Appendix B. Ogg Opus
headers, channel mapping, pre-skip, and granules use RFC 7845. Salsa adds the
constraints in Sections 3 and 4.

The experimental padding carriage uses the long-extension layout of
`draft-ietf-mlcodec-opus-extension-06`. Section 4 defines the Salsa wrapper.
The Opus audio coding algorithm and external library APIs are outside this
specification.

## 2. Notation and fixed geometry

### 2.1 Primitive types

Fields in a syntax table occur in table order. Arrays occur in increasing
index order. All offsets and lengths count **bytes**, unless a unit is given.
There is no implicit alignment, padding, terminator, or native-struct layout.

| Notation | Encoding |
|---|---|
| `u8` | Unsigned 8-bit integer |
| `u16be`, `u32be`, `u64be` | Unsigned integer, most significant byte first |
| `u16le`, `u32le`, `u64le` | Unsigned integer, least significant byte first |
| `i16le` | Signed two's-complement 16-bit integer, little-endian |
| `f32be`, `f64be` | IEEE 754 binary32 or binary64 bits, big-endian |
| `bytes[n]` | Exactly `n` uninterpreted bytes |
| `T[n]` | Exactly `n` consecutive values of type `T` |
| `sym(context)` | One categorical entropy symbol using Section 7 |
| `U(context)` | Unsigned escape-coded integer using Section 7.3 |
| `S(context)` | Signed escape-coded integer using Section 7.3 |
| `B(context)` | Unsigned binary-tail integer using Section 7.3 |

`U` and `S` use the shared `tail` model unless stated otherwise. A flag coded
with `sym` has values 0 and 1. Byte flags emitted by a writer MUST also use
the specified values, even where a reference reader treats any nonzero byte
as true. Reserved bits MUST be zero.

Salsa wrappers and configuration fields are big-endian. Entropy word counts,
entropy words, and correction footer fields are little-endian. Correction
scales are entropy-coded IEEE 754 bit fields, not raw `f32be` values.

### 2.2 Sample clock and dimensions

| Quantity | Value or meaning |
|---|---|
| Sample rate | 48,000 samples per second |
| `N` | Number of non-LFE objects, 0 through 128 |
| `J` | Number of reconstruction observations, 7 |
| `B` | Number of parameter bands, 24 |
| Opus packet duration | 960 samples, 20 ms |
| Normal tile duration | 4,800 samples, five Opus packets |
| STFT length `L` | 1,920 samples |
| STFT hop `H` | 240 samples, 5 ms |
| Nonnegative FFT bins | 961, including DC and Nyquist |
| Maximum correlation pairs | `min(8, floor(N/2))` per tile |
| Position code range | 0 through 4,095 per coordinate |

`frames` means samples **per channel**, not a count of Opus frames or STFT
frames. Private timestamps are absolute positions on the source sample
clock. The first tile can start at a nonzero source position `s0`, for example
when encoding an excerpt. Container playback time zero corresponds to `s0`.
Subsequent tile starts MUST advance by the preceding tile's `frames` value.

The final tile MAY contain fewer than 4,800 source samples. Its final audio
packet is padded to 960 decoded samples. Padding and codec drain samples do
not extend the source programme.

### 2.3 Parameter bands

Band `b` contains the half-open range `[edge[b], edge[b+1])` of nonnegative
FFT bins. The edges are fixed and are not transmitted:

```text
0, 2, 4, 7, 11, 15, 20, 27, 34, 43, 55, 68, 85,
105, 130, 160, 196, 240, 293, 358, 436, 532, 648, 789, 961
```

Bin `k` has frequency `25*k` Hz. The last band includes bin 960 at 24 kHz.

## 3. Ogg Opus transport and channel binding

### 3.1 File organization

The reference file contains one Ogg logical Opus stream:

```text
OpusHead packet
OpusTags packet
audio multistream packets, one per 960 samples
final audio drain packet and EOS trimming
```

Each complete Ogg audio packet contains the concatenated elementary Opus
substreams. Every substream except the last uses Opus self-delimited framing.
The last uses ordinary Opus packet framing. There is no Salsa packet-length
prefix around this concatenation.

The reference writer places one complete packet on each page. Ogg lacing and
continuation are container concerns; they do not add parameter symbols or
reset codec state. A muxer MUST obey the 255-segment Ogg page limit. A packet
too large for one page requires page continuation. See Annex A for the
reference writer's current limit.

### 3.2 `OpusHead`

The initial profile's identification packet is 29 bytes:

| Offset | Field | Type | Value or meaning |
|---:|---|---|---|
| 0 | Signature | `bytes[8]` | ASCII `OpusHead` |
| 8 | Version | `u8` | 1 |
| 9 | Output channel count | `u8` | 8 |
| 10 | Pre-skip | `u16le` | Initial decoded samples to discard |
| 12 | Input sample rate | `u32le` | 48,000 |
| 16 | Output gain | `i16le` | 0 in this profile |
| 18 | Mapping family | `u8` | 1 |
| 19 | Stream count `S` | `u8` | 6 in the initial binding |
| 20 | Coupled stream count `C` | `u8` | 3 in the initial binding |
| 21 | Public channel map | `u8[8]` | `[0,6,1,2,3,8,8,7]` |

The declared public order is `FL, FC, FR, SL, SR, BL, BR, LFE`. It is the
mapping-family-1 order, not the internal observation order and not a host
PCM interleave convention.

`OpusTags` uses the ordinary Opus comment format. The reference vendor is
`salsa-spatial` and the reference writer emits zero comments. Vendor text is
not authoritative identification. A remuxer can change the vendor without
changing Salsa records.

### 3.3 Coded, logical, and observation channels

For `S` substreams and `C` coupled substreams, the coded channel count is
`K = S + C`. Coupled substream `j < C` produces coded channels `2*j` and
`2*j+1`. Mono substream `j >= C` produces coded channel `j+C`.

The initial layout is:

| Substream | Channels | Coded indices | Private logical indices | Meaning |
|---:|---:|---|---|---|
| 0 | 2 | 0, 1 | 0, 1 | Folded FL, FR |
| 1 | 2 | 2, 3 | 4, 5 | Folded SL, SR |
| 2 | 2 | 4, 5 | 6, 7 | Auxiliary `H_L`, `H_R` |
| 3 | 1 | 6 | 2 | FC; metadata carrier |
| 4 | 1 | 7 | 3 | LFE |
| 5 | 1 | 8 | 8 | Shared rear silence |

Private logical order is `FL, FR, FC, LFE, SL, SR, H_L, H_R, silence`.
The private permutation `T = logical_to_coded` is initially
`[0,1,6,7,2,3,4,5,8]`. With `Z` the last logical index, the public map MUST be:

```text
[T[0], T[2], T[1], T[4], T[5], T[Z], T[Z], T[3]]
```

The two rear outputs reference the same encoded zero-input signal. Auxiliary
coded channels MUST NOT appear in the public map. In particular, the public
BL and BR outputs do not contain the height observations.

The enhanced reconstruction input order is
`FL, FR, FC, SL, SR, H_L, H_R`, corresponding to logical indices
`[0,1,2,4,5,6,7]`. LFE and shared silence are not reconstruction observations.
An enhanced decoder MUST recover the hidden coded channels before applying
the private routing. The eight public speaker outputs alone are insufficient.

### 3.4 Metadata carrier and packet timing

The carrier is the mono center substream. Its coded index is public map
entry 1. Its substream index is `channel_mapping[1] - C`; the coded index
MUST be at least `2*C`. The initial carrier index is 3.

All substreams in one multistream packet MUST have the same 960-sample
duration. A full tile consists of five such packets. Profile writers MUST
attach the tile's record bundle to the first packet of the tile. The reference
writer uses one fragment. The fragment syntax also permits splitting a bundle
across packets of the same tile, provided all fragments arrive before the
tile is committed.

### 3.5 Pre-skip and end trimming

Opus pre-skip applies once at programme start. It MUST NOT be applied again
at a parameter checkpoint. The source-aligned sample at offset `n` follows
the first `pre_skip` raw decoded samples and has private timestamp `s0+n`.
The tested restricted-low-delay encoder uses 120 samples of lookahead;
120 is an implementation value, not a substitute for reading `OpusHead`.

An EOS granule defines the container end position. After pre-skip, the
container duration is `final_granule - pre_skip` samples. A conforming muxer
MUST exclude padding and drain samples from that duration. A drain packet
carries audio needed to recover the delayed tail but no new parameter tile.
Private tile durations remain the source-duration authority for enhanced
output. Annex A describes the current writer/parser discrepancy for final
partial packets and drain packets.

## 4. Private extension, fragments, and records

### 4.1 Layer order

The nested structure is:

```text
Ogg audio packet
  Opus multistream packet
    mono center elementary packet, code 3 with padding
      experimental long extension, ID 120
        SALS wrapper
          SLSR fragment
            complete bundle after fragment reassembly
              SLEN configuration record -> SCFG payload
              SLEN checkpoint record    -> independent entropy stream
              SLEN tile record          -> STIL payload
```

Configuration and checkpoint records are conditional. The tile record is
present for each enhanced tile. No separate serialized object-event list
occurs in `STIL`.

### 4.2 Opus padding extension

The extension instance starts with `0xF1`, which encodes `(120 << 1) | 1`.
The low bit selects an explicit length. The following length is the sum of
successive bytes: each `255` continues the length; the first byte below 255
terminates it and contributes its value. Thus 255 bytes is encoded `FF 00`.
Exactly that many bytes form the `SALS` wrapper.

This length coding differs from the surrounding **Opus padding length**:
an Opus padding-header byte of 255 contributes 254, then continues. A writer
MUST use the correct rule at each layer.

To add padding, the writer preserves the Opus frame payloads and converts
the packet to code 3. The reference representation sets VBR and padding bits
in the frame-count byte. For one frame that byte is `0xC1`. The TOC's upper
six bits remain unchanged. Salsa metadata belongs to padding, so it is not
part of the 1,275-byte limit on an individual Opus audio frame.

### 4.3 `SALS` wrapper

| Offset | Field | Type | Meaning |
|---:|---|---|---|
| 0 | Magic | `u32be` | `0x53414C53`, ASCII `SALS` |
| 4 | Version | `u16be` | 1 |
| 6 | Flags | `u8` | Original packet framing, below |
| 7 | `original_padding_header_length` | `u16be` | Number of saved padding-header bytes |
| 9 | `original_padding_length` | `u32be` | Number of saved padding bytes |
| 13 | `payload_length` | `u32be` | Number of Salsa fragment bytes |
| 17 | Original padding header | `bytes[original_padding_header_length]` | Original code-3 padding-length bytes |
| variable | Original padding | `bytes[original_padding_length]` | Original padding payload |
| variable | Salsa payload | `bytes[payload_length]` | One `SLSR` fragment |

Flags bits 0-1 contain the original Opus packet code. Bit 2 records the
original code-3 VBR flag. Bit 3 records whether the original code-3 packet
had a padding header. Bits 4-7 are zero. For original packet codes 0, 1, and
2, the saved padding fields are empty and bits 2-3 are zero.

The wrapper permits the original elementary packet to be reconstructed
without the Salsa extension. Original code 0 requires one frame; code 1
requires two equal-sized frames; code 2 requires two frames with an explicit
first-frame size. Code 3 restores its saved flags and padding. Extension
stripping MUST preserve the audio frame bytes.

### 4.4 `SLSR` fragment header

The fixed header is 30 bytes:

| Offset | Field | Type | Meaning |
|---:|---|---|---|
| 0 | Magic | `u32be` | `0x534C5352`, ASCII `SLSR` |
| 4 | Record version | `u16be` | 1 |
| 6 | Wrapper type | `u8` | 0 |
| 7 | Reserved | `u8` | 0 |
| 8 | `config_id` | `u32be` | Configuration association |
| 12 | `tile_start_sample` | `u64be` | Source position of the tile |
| 20 | Fragment index | `u8` | Zero-based index |
| 21 | Fragment count | `u8` | 1 through 64 |
| 22 | `total_length` | `u32be` | Complete record-bundle length |
| 26 | `fragment_length` | `u32be` | Payload length in this fragment |
| 30 | Fragment payload | `bytes[fragment_length]` | Consecutive bundle bytes |

Reassembly is keyed by `(config_id, tile_start_sample)`. All fragments for
one key MUST agree on count and total length. Each index MUST occur once.
The index MUST be less than the count. Concatenation in index order MUST
contain exactly `total_length` bytes. The complete bundle MUST NOT exceed
1,048,576 bytes. Fragments have no independent record type; typed records
begin only after bundle reassembly.

The reference reassembler retains one key at a time. A new key replaces an
incomplete old bundle. A valid stream MUST NOT interleave fragment bundles
that require the reader to retain multiple keys.

### 4.5 `SLEN` typed record

Records are concatenated without separators. Each has a 12-byte header:

| Offset | Field | Type | Meaning |
|---:|---|---|---|
| 0 | Magic | `u32be` | `0x534C454E`, ASCII `SLEN` |
| 4 | Record version | `u16be` | 1 |
| 6 | Type | `u8` | 1 configuration, 2 tile, 3 checkpoint |
| 7 | Flags | `u8` | 0; no required/optional flag is defined |
| 8 | Payload length | `u32be` | Excludes the 12-byte header |
| 12 | Payload | `bytes[payload_length]` | Type-specific syntax |

Payload length MUST NOT exceed 1,048,576 bytes and MUST fit within the bundle.
A reader MUST skip an unknown record type by its length. The current record
version has no mechanism for declaring an unknown type mandatory. An unknown
record version cannot be interpreted as version 1.

A valid bundle contains at most one configuration, one checkpoint, and one
tile record, in that order with absent records omitted. The fragment key,
configuration ID, checkpoint timestamp, and tile timestamp MUST identify the
same configuration/tile where those fields occur. Unknown optional records
MAY appear between known records.

## 5. Configuration record

### 5.1 Field sequence

The type-1 record payload is:

| Field | Type | Meaning |
|---|---|---|
| Magic | `u32be` | `0x53434647`, ASCII `SCFG` |
| `format_version` | `u16be` | 4 |
| `profile` | `u8` | 1 |
| `config_id` | `u32be` | Matches the fragment association |
| `object_count` | `u32be` | `N`, excluding LFE |
| `headroom_gain` | `f64be` | Finite positive linear gain already applied to audio |
| `core_bitrates` | `u32be[5]` | Encoder targets: front, surround, auxiliary, center, LFE, in bits/s |
| `beta` | `f64be` | Encoder metadata RD weight |
| Codec binding | Structure | Section 5.2 |
| `signal_model_count` | `u32be` | Number of model descriptors |
| Signal models | Structure array | Section 5.3 |
| `observation_group_count` | `u32be` | Number of auxiliary groups |
| Observation groups | Structure array | Section 5.3 |
| `base_fold_present` | `u8` | 0 or 1 |
| Base fold, if present | Structure | Section 5.4 |
| `reconstruction_stage_count` | `u32be` | Number of reconstruction stages |
| Reconstruction stages | Structure array | Section 5.5 |
| `initial_object_count` | `u32be` | 0 or `N` |
| Initial objects | Structure array | Section 5.6 |
| `initial_trajectory_count` | `u32be` | 0 or `N` |
| Initial trajectories | Structure array | Section 5.7 |
| `ceiling_horizontal_gain` | `f64be` | Finite `rho` in `[0,1)` |

The bitrate targets and `beta` describe encoder choices. They MUST NOT affect
parameter decoding, packet duration, or reconstruction gain. A decoder MUST
use actual packet lengths for parsing. The ceiling gain is always present in
version 4, including when zero.

The first full configuration supplies the initial scene. Later descriptor
resends MAY set both initial-scene counts to zero when an accompanying
checkpoint supplies the current trajectories. Such a resend MUST NOT erase
the current scene. If a checkpoint leaves an object's trajectory empty, the
full initial scene remains necessary as that object's fallback.

### 5.2 Codec-binding structure

| Field | Type | Meaning |
|---|---|---|
| `output_channels` | `u32be` | 8 |
| `mapping_family` | `u8` | 1 |
| `stream_count` | `u32be` | `S` |
| `coupled_count` | `u32be` | `C` |
| `channel_mapping_length` | `u32be` | 8 |
| `channel_mapping` | `u8[channel_mapping_length]` | Public map |
| `logical_to_coded_length` | `u32be` | `K = S+C` |
| `logical_to_coded` | `u8[logical_to_coded_length]` | Permutation `T` |

`S` MUST be positive, `C <= S`, and `K <= 255`. `T` MUST be a permutation
of `0..K-1`. Logical channels 0-5 are the fallback core, the last logical
channel is shared silence, and any intervening channels are auxiliary.
This structure can express up to 248 auxiliary channels; the defined
reconstruction profile uses two.

The five public-header fields (output count, family, stream count, coupled
count, and public map) MUST match `OpusHead`. The public map MUST satisfy
Section 3.3. Public map entry 255, the generic Opus silent-channel marker,
is not used here; rear silence is an actual coded signal.

### 5.3 Models and auxiliary groups

Each signal model contains:

| Field | Type | Meaning |
|---|---|---|
| `model_ref` | `u32be` | Unique model identifier |
| `algorithm` | `u8` | 1 for the Salsa core panner; 0 is invalid |
| `algorithm_version` | `u16be` | 1 or 2 for the defined panner |

Other nonzero algorithm tags can be parsed as unknown algorithms. A required
unknown algorithm or version disables enhanced reconstruction; it MUST NOT
select a different panner by approximation.

Each observation group contains:

| Field | Type | Meaning |
|---|---|---|
| `group_id` | `u32be` | Unique group identifier |
| `logical_channel_count` | `u32be` | Number of output rows in the group |
| `logical_channels` | `u32be[logical_channel_count]` | Auxiliary logical indices, in row order |
| `input_domain` | `u8` | 1 scene objects; 2 residual of an earlier stage |
| `input_stage_id`, only for domain 2 | `u8` | Referenced stage |
| `projection` | `u8` | 1: select model rows |
| `model_ref` | `u32be` | Referenced signal model |
| `row_count` | `u32be` | Equals `logical_channel_count` |
| `rows` | `u8[row_count]` | Ordered model-row tags |

Row tags are `0=L`, `1=R`, `2=C`, `3=Ls`, `4=Rs`, `5=H_L`, and `6=H_R`.
Each auxiliary logical channel MUST belong to exactly one group. All model
and required-group references MUST resolve. Unknown input-domain tags,
projection tags, or row tags have no defined length-skipping syntax.

The initial group has ID 1, logical channels `[6,7]`, input domain 1,
projection 1, model reference 1, and rows `[5,6]`. Residual input domain 2
has a serialized representation but no reconstruction algorithm in this
profile.

### 5.4 Base fold

When `base_fold_present` is 0, the fold matrix is zero. When it is 1,
the following fields occur:

| Field | Type | Meaning |
|---|---|---|
| `preset` | `u8` | 1 for the defined fallback fold |
| `matrix` | `f64be[5][2]` | Row-major, finite coefficients |

Rows are `FL, FR, FC, SL, SR`; columns are `H_L, H_R`. Preset 1 is:

```text
0.5  0.0
0.0  0.5
0.0  0.0
0.5  0.0
0.0  0.5
```

The 80 matrix bytes are present even though preset 1 fixes their values.
Writers using preset 1 MUST emit this matrix. A present zero matrix with
preset 0 is the explicit identity/no-fold form used by helper APIs. Other
preset meanings are not defined here. The decoder uses the serialized matrix
for `D_base = D_ground + A*D_height` and reconstructs from the folded base
observations.

### 5.5 Reconstruction-stage structure

| Field | Type | Meaning |
|---|---|---|
| `stage_id` | `u8` | Stage identifier |
| `base_count` | `u32be` | Number of base observations |
| `base_logical` | `u32be[base_count]` | Ordered base logical indices |
| `required_group_count` | `u32be` | Number of required groups |
| `required_groups` | `u32be[required_group_count]` | Group identifiers |
| `observation_count` | `u32be` | Total observations |
| `observation_logical` | `u32be[observation_count]` | Complete observation order |

The defined stage has ID 1, base `[0,1,2,4,5]`, required group `[1]`, and
observation order `[0,1,2,4,5,6,7]`. The stage MUST contain five full-range
base observations and the two height observations. LFE and silence MUST NOT
be included. Model and group IDs are references, not executable names.

The initial model is reference 1 with algorithm 1. Panner version 1 requires
`rho=0`. Version 2 uses the transmitted `rho`, including zero. Nonzero `rho`
with a version-1 panner is invalid.

### 5.6 Initial-object entry

| Field | Type | Meaning |
|---|---|---|
| `id` | `u16be` | Object identifier; the profile writer uses `index+1` |
| `compact_position` | `u8` | 1 for quantized coordinates, 0 for raw coordinates |
| Position, if compact | `u16be[3]` | 12-bit codes in 16-bit containers, x then y then z |
| Position, otherwise | `f32be[3]` | Finite x, y, z coordinates |
| `gain` | `f32be` | Initial object metadata; profile writer emits 1 |
| Flags | `u8` | Bit 0 enabled; bit 1 size present; bit 2 spread present |
| `size`, if present | `f32be` | Initial metadata scalar |
| `spread`, if present | `f32be` | Initial metadata scalar |

Flag bits 3-7 are zero. A compact entry without size or spread is 14 bytes;
a raw-position entry without those fields is 20 bytes. Object indexing in
parameter and correction syntax uses table positions, not this `id` field.
For the defined reconstruction profile, IDs are `1..N`, gain is 1, and size
and spread are absent. The reference DSP does not apply size, spread, initial
gain, or initial enabled state as additional reconstruction controls.

Authored gain and activity have already been applied to source PCM before
the transported signals are formed. An enhanced decoder MUST NOT apply
those authored controls a second time. Parameter activity has the distinct
covariance meaning in Section 11.2.

### 5.7 Initial trajectories and position codes

Each initial trajectory starts with `knot_count: u32be`. The following
structure repeats for each knot:

| Field | Type | Meaning |
|---|---|---|
| `compact` | `u8` | 0 or 1 |
| `sample_delta`, if compact | `u32be` | Nonnegative delta from the previous knot sample |
| Position, if compact | `u16be[3]` | x, y, z position codes |
| `sample`, otherwise | `u64be` | Absolute source sample |
| Position, otherwise | `f64be[3]` | Finite x, y, z coordinates |

The previous sample starts at zero for each trajectory. Either form updates
it. Compact and raw knots can be mixed in one trajectory. A compact knot is
11 bytes; a raw knot is 33 bytes, excluding the trajectory's count.

Coordinates use x toward the right, y toward the front, and z upward:

```text
x(code) = -1 + 2*code/4095
y(code) = -1 + 2*code/4095
z(code) =      code/4095
```

Each compact code MUST be in `0..4095`. The forward quantizer clamps the
coordinate to its range, scales to `0..4095`, and rounds to the nearest
integer, with a half rounded upward. Code 2048 is slightly above zero for
x and y; exact zero has no code on those axes.

Trajectory knots MUST be in nondecreasing sample order. Equal sample times
represent an immediate replacement: the last knot at or before the queried
sample supplies the active position. Between distinct sample times, position
is linearly interpolated. Before the first knot and after the last, position
is held. Initial trajectories establish geometry; they do not seed the
entropy predictor. Section 8 defines the separate predictor state.

## 6. Tile record

### 6.1 `STIL` payload

The type-2 record has a 32-byte payload header:

| Offset | Field | Type | Meaning |
|---:|---|---|---|
| 0 | Magic | `u32be` | `0x5354494C`, ASCII `STIL` |
| 4 | `start_sample` | `u64be` | Tile source position |
| 12 | `frames` | `u32be` | Source samples per channel |
| 16 | `enhancement_start_sample` | `u64be` | Same value as `start_sample` |
| 24 | `enhancement_frames` | `u32be` | Same value as `frames` |
| 28 | `enhancement_length` | `u32be` | Parameter bytes, including any correction trailer |
| 32 | Enhancement | `bytes[enhancement_length]` | Sections 7, 8, and 10 |

`frames` MUST be in `1..4800`. Only the final source tile can be shorter
than 4,800 samples. A tile's audio needs `ceil(frames/960)` packets, excluding
an optional final codec drain packet. The duplicated timestamps and durations
MUST agree. No extra bytes follow the declared enhancement in this record.

An enhancement length of zero denotes unavailable enhancement and invokes
Section 12 recovery behavior. It is not an entropy encoding of an empty scene.

### 6.2 Enhancement byte layout

```text
u32le main_word_count
u32le main_words[main_word_count]
optional correction payload and footer
```

The main entropy block occupies exactly `4 + 4*main_word_count` bytes.
If a correction is present, it begins immediately after that block and ends
at the enhancement boundary. The correction's own entropy block has an
independent word count. No padding or unnamed suffix is defined between
these blocks.

## 7. Entropy coding

### 7.1 Probability models

Each context owns positive integer counts `c[s]`. The cumulative interval
total is `F = 2^24`. Before coding a symbol, compute frequencies as follows:

1. Let `C` be the sum of the context's counts.
2. Set `f[s] = max(1, floor(c[s]*F/C))` for each symbol.
3. If the frequency sum is below `F`, add one unit at a time in ascending
   symbol-index order, wrapping to index zero if necessary.
4. If the sum is above `F`, remove one unit at a time in descending
   symbol-index order from entries greater than one, wrapping as necessary.
5. Set `cdf[s]` to the sum of `f[0..s-1]`.

After each symbol, increment its count by one. If the new count sum is at
least 4,096, replace every count with `max(1, ceil(count/2))`. The update occurs
after the symbol's interval has been used.

The priors below are used throughout this specification:

| Prior | Counts |
|---|---|
| `flag` | `[1,1]` |
| `uniform(K)` | `K` counts, each 1 |
| `innovation` | 33 counts: `c[s] = 2^max(0,6-s)` for `s=0..32` |

An encoder records intervals in logical decoding order, with these adaptive
updates, before encoding the intervals in reverse order into the rANS stack.

### 7.2 rANS word representation

The coder has a 64-bit unsigned state `X` and a stack of 32-bit words.
The implementation uses `constriction` 0.5.0 `DefaultAnsCoder`; the following
rules specify the required representation without requiring that library.

To encode an interval `(c,f)`:

```text
if (X >> 40) >= f:
    append low32(X) to the word stack
    X = X >> 32
X = (floor(X/f) << 24) + (X mod f) + c
```

Start with `X=0`. Before the reversed payload symbols, encode a fixed marker
with `c=2^23` and `f=2^23`. After encoding all payload symbols, append the
nonzero state's 32-bit chunks in low-to-high order, omitting a zero high
chunk. Serialize the total word count as `u32le`, then all words as `u32le`
in append order.

For decoding, pop the last word into `X`; that word MUST be nonzero. If
another word remains, append it to the low end of `X` by
`X = (X << 32) | pop()`. For each decoded symbol:

```text
q = X mod 2^24
find s such that cdf[s] <= q < cdf[s] + f[s]
X = f[s]*(X >> 24) + q - cdf[s]
if X < 2^32 and a stack word remains:
    X = (X << 32) | pop()
update the selected context with s
```

After the syntax's last symbol, decode the fixed binary marker using counts
`[1,1]` without adapting a payload context. The marker MUST be 1. The final
state MUST be zero and no stack words may remain. This check validates the
rANS termination; it is not a checksum of the metadata.

A generic entropy stream with no symbols has zero bytes. Actual parameter
tiles, checkpoints, and correction payloads each contain required symbols,
so their entropy blocks are nonempty. A word count is a framing value, not
an entropy-coded symbol.

### 7.3 Integer symbols and escape tails

`B(context)` represents a nonnegative 64-bit integer `v` using a binary
adaptive model. Let `n` be the bit length of `v`, with `n=0` for zero.
Emit `n` one symbols, then one zero symbol. If `n>0`, emit the remaining
`n-1` bits of `v`, most significant first; the leading one is implicit.
Every unary and payload bit updates the same binary context. More than 64
unary ones is invalid.

`U(context)` uses an `innovation` context with direct symbols 0 through 31.
Values below 32 use one direct symbol. A value of 32 or more uses symbol 32,
then `B(tail)` for `v-32`.

`S(context)` first maps a signed integer `v` to a nonnegative integer:

```text
zigzag(v) = 2*v       if v >= 0
zigzag(v) = -2*v - 1  if v < 0
```

It then uses the same direct/escape rule as `U`. Thus the direct signed
range is `-16..15`; `+16` is the first positive escape. The integer state
updates specified below MUST be evaluated without overflow before range
checks. Timestamp predictors operate in the nonnegative signed-64-bit range.

### 7.4 Main parameter contexts

Each row is a separate model. An indexed row creates one model per index,
not one model shared by the whole array.

| Context | Prior | Use |
|---|---|---|
| `anchor_count`, `anchor_time` | `innovation`, each | Tile anchor count and signed sample deltas |
| `activity_change`, `activity_value` | `flag`, each | Object activity updates |
| `power_delta[b]`, 24 models | `innovation` | Power deltas; shared across objects within band `b` |
| `error_mask_change`, `error_mask_value` | `flag`, each | Error mask updates |
| `error_innovation[j]`, 7 models | `innovation` | Error deltas; shared across bands of observation `j` |
| `correlation_count` | `uniform(min(8,floor(N/2))+1)` | Number of pairs |
| `correlation_pair_a`, `correlation_pair_b` | `uniform(max(N,1))`, each | Object indices |
| `correlation_present` | `flag` | Per-band coefficient presence |
| `correlation_magnitude[b]`, 24 models | `innovation` | Magnitude deltas |
| `correlation_phase[b]`, 24 models | `innovation` | Phase deltas |
| `trajectory_update` | `flag` | Per-object trajectory replacement |
| `trajectory_count`, `trajectory_time`, `trajectory_position_delta` | `innovation`, each | Knot count, time delta, coordinate-code delta |
| `tail` | `flag` | All escaped integer tails in this entropy stream |

These contexts persist across tiles. A checkpoint installation resets them
to the priors. Each tile nevertheless starts a new rANS state and has its
own length-prefixed words. Adaptive context lifetime and rANS state lifetime
are different.

## 8. Parameter tile syntax

### 8.1 Predictor initialization

A new parameter decoder creates the following state:

| State | Initial value |
|---|---|
| `active[i]` | false |
| `ell[i][b]` | 0, in half-dB units |
| `error_log[j][b]` | 0, in 2-dB units |
| `unobservable[j][b]` | false |
| Previous pair list | Empty |
| Previous trajectory for each object | Empty |

Initial configuration positions establish the DSP geometry fallback. The
trajectory predictor starts empty as specified in the table. A checkpoint
replaces the predictor with its transmitted state.

### 8.2 Logical symbol order

The entire main payload is one entropy stream in this order:

```text
anchor_count = U(anchor_count)
for each anchor:
    sample_delta = S(anchor_time)
    for object i in 0..N-1:
        activity_changed = sym(activity_change)
        if activity_changed:
            active[i] = sym(activity_value)
    for object i in 0..N-1:
        if active[i]:
            for band b in 0..23:
                ell_delta = S(power_delta[b])
        else:
            reset ell[i][0..23] to zero
    for observation j in 0..6:
        for band b in 0..23:
            mask_changed = sym(error_mask_change)
            if mask_changed:
                unobservable[j][b] = sym(error_mask_value)
            error_delta = S(error_innovation[j])
pair_count = sym(correlation_count)
for each pair:
    pair syntax from Section 8.5
for object i in 0..N-1:
    trajectory syntax from Section 8.6
fixed rANS termination marker
```

All activity flags for an anchor precede all power symbols. Error symbols
are observation-major, then band-major.

### 8.3 Anchor times and powers

The anchor-time predictor starts at the current tile's `start_sample`.
Each `sample_delta` is added to the preceding anchor sample. Decoded times
MUST be nonnegative and MUST NOT overflow. Profile anchors MUST be ordered
and refer to the tile's source interval. The parser's allocation bound is
100,000 anchors; a useful enhanced tile contains at least one anchor.
The syntax carries sample deltas. The regular 5-ms-grid schedule in Annex A
is an encoder choice.

If `activity_changed` is zero, the previous activity is retained. Otherwise
the following value replaces it. Inactive objects emit no power symbols and
their 24 power predictors become zero.

For each active object and band, set:

```text
ell[i][b] = previous_ell[i][b] + S(power_delta[b])
```

The sum MUST fit in a signed 32-bit integer. One integer unit is 0.5 dB.
The unit remains 0.5 dB for all encoder quantizers. The decoder applies the
90-dB reconstruction bound when converting this predictor to normalized
power in Section 11.2.

### 8.4 Core-error envelope

For every observation and band, decode the mask change and optional mask
value. Decode an error delta **even if the mask is set**. Then update:

```text
error_log[j][b] = clamp(previous_error_log[j][b] + error_delta, -160, 160)
nu[j][b] = 0                                  if unobservable[j][b]
nu[j][b] = 0                                  if error_log[j][b] == -160
nu[j][b] = 10^(2*error_log[j][b]/10)            otherwise
```

The pre-clamp sum MUST fit in a signed 32-bit integer. Code -160 is an exact
zero sentinel. Other codes represent linear power ratios at 2-dB spacing.
An encoder sets the transmitted log value to -160 for an unobservable entry.

`nu` is the dimensionless ratio of observation error power to total ideal
full-range core power in that band. The per-observation masks control the
band mute defined in Section 11.3.

### 8.5 Correlation pairs

`pair_count` is an absolute categorical symbol. Each pair is:

```text
a = sym(correlation_pair_a)
b = sym(correlation_pair_b)
for band k in 0..23:
    present = sym(correlation_present)
    if present:
        magnitude_delta = S(correlation_magnitude[k])
        if reconstructed_magnitude > 0:
            phase_delta = S(correlation_phase[k])
```

`a` and `b` MUST be distinct valid object indices. No object can occur in
two pairs in the tile. Pair order and the orientation `(a,b)` are preserved
for prediction; the predictor key includes both indices in their encoded order.

The predictor is the previous tile's coefficient for the exact ordered pair
and band. An absent previous coefficient has magnitude and phase zero.
Add the magnitude delta to the previous magnitude. The result MUST be in
`0..15`. A zero magnitude emits no phase symbol and reconstructs phase zero.
For a positive magnitude, the previous phase is used only if the previous
coefficient also had positive magnitude; otherwise the phase predictor is
zero. The new phase is `(previous_phase + phase_delta) mod 32`.

Writers use the wrapped phase delta in `-16..15`. The reconstructed complex
coefficient is:

```text
rho_ab = (magnitude/16) * exp(i*2*pi*phase/32)
```

Presence zero clears the coefficient in that band. The new list replaces
the previous pair list after all pairs have been
decoded. The same tile pair list applies to every anchor in that tile.

### 8.6 Trajectory updates

For each object, decode `sym(trajectory_update)`. Zero retains the complete
previous knot list. One is followed by `U(trajectory_count)` and a replacement
list of that many knots. Writers emit updates for nonempty changed lists;
an empty encoder input list means no update. The decoder permits a replacement
count of zero. A list is bounded to 100,000 knots.

For a replacement, initialize the time predictor from the **last knot of the
old list**, or the tile start if the old list is empty. Initialize each
coordinate predictor from the quantized last old position, or code zero if
the old list is empty. The all-zero coordinate codes represent `(-1,-1,0)`.

For each new knot, in order:

1. Decode `S(trajectory_time)` and add it to the previous sample.
2. For x, y, then z, decode `S(trajectory_position_delta)`.
3. Add each delta to its previous coordinate code and clamp to `0..4095`.
4. Reconstruct the position using Section 5.7 and advance both predictors.

The new knot time MUST be nonnegative and fit in signed 64-bit arithmetic.
The first delta can be negative because a replacement list can overlap the
previous list. The reconstructed list replaces the old list. Geometry uses
the interpolation rule in Section 5.7.

## 9. Independent checkpoint syntax

### 9.1 Record payload and context lifetime

A type-3 record payload consists of one Section 7 entropy block. Its first
value is the integer `0x53434B51` (`SCKQ`), coded with `B(snapshot_bits)`.

Each checkpoint uses fresh contexts. Its contexts are discarded after
decoding and MUST NOT become the following tile's contexts. The following
tile starts with fresh main-parameter contexts and the checkpoint's restored
predictor values.

| Checkpoint context | Prior |
|---|---|
| `snapshot_bits`, `snapshot_flag`, `tail` | `flag`, each |
| `snapshot_trajectory`, `snapshot_position_delta` | `innovation`, each |
| `snapshot_ell`, `snapshot_error` | `innovation`, each |

These priors do not depend on `N`; a checkpoint can be parsed before its
object count is known. `B(snapshot_bits)` shares one adaptive binary model
across all the fields that use it, starting with the magic.

### 9.2 Logical symbol sequence

| Field or loop | Encoding | Meaning |
|---|---|---|
| Magic | `B(snapshot_bits)` | `0x53434B51` |
| `reset_predictors` | `sym(snapshot_flag)` | Stored flag; installation semantics below |
| `start_sample` | `B(snapshot_bits)` | State boundary immediately before the tile |
| `object_count` | `B(snapshot_bits)` | `N <= 128` |
| `active[i]`, for each object | `sym(snapshot_flag)` | Full activity state |
| `trajectory_count` | `B(snapshot_bits)` | Must equal `N` for installation |
| For each trajectory: `knot_count` | `B(snapshot_bits)` | At most 100,000 |
| For each knot: sample delta | `S(snapshot_trajectory)` | Previous sample starts at zero per trajectory |
| For each knot: x, y, z code deltas | `S(snapshot_position_delta)` | Previous codes start at zero per trajectory |
| `power_object_count` | `B(snapshot_bits)` | Must equal `N` for installation |
| For each object, then each band: `ell` | `S(snapshot_ell)` | Absolute half-dB predictor, including inactive objects |
| For each observation, then each band: `error_log` | `S(snapshot_error)` | Absolute error predictor |
| After each `error_log`: unobservable flag | `sym(snapshot_flag)` | Full error-mask state |
| `pair_count` | `B(snapshot_bits)` | Full previous-tile pair list |
| For each pair: `a`, then `b` | `B(snapshot_bits)` each | Object indices |
| For each pair, then each band: present | `sym(snapshot_flag)` | Coefficient presence |
| If present: magnitude, then phase | `B(snapshot_bits)` each | Absolute codes; phase is present even at magnitude zero |
| Terminator | Fixed rANS marker | Section 7.2 |

Trajectory reconstruction uses the same addition, coordinate clamping, and
sample validation as Section 8.6, with the checkpoint-specific initial
predictors in this table. Power and error predictor values MUST fit signed
32-bit integers. Checkpoint pair indices, disjointness, magnitude, and phase
MUST satisfy the same semantic constraints as a tile pair list.

### 9.3 Installation

The checkpoint describes state immediately **before** its associated tile.
Its object count MUST equal the active configuration count. Its activity,
power, and trajectory arrays MUST all have that count. Its timestamp MUST
equal the tile start. Installation MUST be atomic.

Installation replaces activity, power, error, pair, and trajectory predictors
and resets the main entropy contexts to their priors. The stored
`reset_predictors` flag is informational. Installation performs the same
operation for either flag value. The current writer emits zero.

An empty checkpoint trajectory installs an empty predictor, but the DSP
geometry can retain the established initial/current fallback position.
For independent entry without earlier geometry, writers SHOULD provide
nonempty trajectories for every object or resend a full initial scene.

The checkpoint contains no Opus decoder state, STFT overlap, covariance
timeline, or correction keyframes. It therefore establishes independent
**parameter** decoding, not a sample-exact audio restart. Section 12 describes
the separate audio preroll requirement.

## 10. Reconstruction correction syntax

### 10.1 Trailer envelope

When present, the correction occupies the bytes after the main entropy
block:

```text
u8    correction_version = 4
u32le correction_word_count
u32le correction_words[correction_word_count]
u32le correction_payload_length
u32le correction_trailer_magic = 0x53435254
```

The payload length includes the version byte and correction entropy block,
but excludes the final eight bytes. Thus
`correction_payload_length = 1 + 4 + 4*correction_word_count`.
The footer magic is written little-endian as bytes `54 52 43 53` (`TRCS`).

The final eight enhancement bytes locate the correction. A matching magic
requires a valid length and version. A malformed present correction rejects
the enhanced tile in the reference full decoder. An absent trailer supplies
no correction keyframes; it does not insert a zero keyframe or reset the DSP
correction timeline.

### 10.2 Correction contexts

All models reset for each correction payload and persist through all its
bands and keyframes. They are separate from main-parameter and checkpoint
models.

| Context | Prior |
|---|---|
| `keyframe_count` | `uniform(9)`, values 0 through 8 |
| `band_present`, `per_keyframe_rank`, `shared` | `flag`, each |
| `rank` | `uniform(8)`, values 0 through 7 |
| `support_len` | `uniform(33)`, values 0 through 32 |
| `support_first`, `support_gap` | `uniform(128)`, each |
| `left`, `right` | `uniform(15)`, each |
| `scale_sign` | `flag` |
| `scale_exponent`, `scale_mantissa_low`, `scale_mantissa_mid` | `uniform(256)`, each |
| `scale_mantissa_high` | `uniform(128)` |

All values in the correction use `sym`; there are no integer escape tails.

### 10.3 Band and keyframe order

Decode `Kf = sym(keyframe_count)`, then visit bands 0 through 23. For each
band, decode `sym(band_present)`. Zero supplies an empty correction for this
band at every keyframe. A present band requires `Kf>0` and the following:

```text
per_keyframe_rank = sym(per_keyframe_rank)
if per_keyframe_rank:
    for keyframe in 0..Kf-1:
        rank = sym(rank)
        for component in 0..rank-1:
            support_and_left()
            right()
else:
    rank = sym(rank)
    for component in 0..rank-1:
        shared = sym(shared)
        if shared:
            support_and_left()
        for keyframe in 0..Kf-1:
            if not shared:
                support_and_left()
            right()
```

The shared form reuses support, left factors, and left scale for that
component across keyframes. Right factors and their scale are always coded
for each keyframe. In the equal-rank form the component loop encloses the
keyframe loop. In the varying-rank form those loops have the opposite order.

### 10.4 Component fields

`support_and_left()` contains:

1. `sym(support_len)` giving the support length `L <= 32`.
2. `L` object-index symbols. The first uses `support_first` and is absolute.
   Each later symbol uses `support_gap` and reconstructs
   `index = previous_index + 1 + gap`.
3. `L` complex left values, each real symbol then imaginary symbol using
   the same `left` model.
4. One left-scale field using Section 10.5.

Support indices MUST be strictly increasing, less than 128, and less than
the current object count. A length of zero is an empty component. There is
no support-index-to-object-ID lookup.

`right()` contains seven complex right values in observation order, then
one right-scale field. Each complex value uses real then imaginary symbols
from the `right` model. For both left and right factors, a symbol `s` in
`0..14` reconstructs `(s-7)/7`. The real and imaginary coordinates are
quantized separately on a 15-level scalar grid. Each coordinate occupies
one entropy symbol.

For object `i` and observation `j`, a component contributes:

```text
delta_M[i][j] += (left[i] * left_scale) * (right[j] * right_scale)
```

Objects outside the support get zero contribution. The stored right factor
already includes the encoder's transpose/conjugation convention. The decoder
MUST use the product above.

### 10.5 Scale field

A scale is one IEEE 754 binary32 value split into five entropy symbols:

| Order | Context | Source bits |
|---:|---|---|
| 1 | `scale_sign` | Bit 31 |
| 2 | `scale_exponent` | Bits 30-23 |
| 3 | `scale_mantissa_low` | Bits 7-0 |
| 4 | `scale_mantissa_mid` | Bits 15-8 |
| 5 | `scale_mantissa_high` | Bits 22-16 |

Reassemble these bits, interpret them as binary32, then convert to the DSP
working precision. Scales MUST be finite. Zero and negative finite scales
are representable. No prediction from the previous scale occurs.

### 10.6 Keyframe times

For tile start `s`, duration `T`, and `Kf>0`, correction keyframe `k` occurs at:

```text
t[k] = s + floor(T * (2*k + 1) / (2*Kf)),  k = 0..Kf-1
```

There are no keyframe timestamps in the payload. Materialize each keyframe's
dense matrix by summing its components. Interpolate those dense matrices
linearly in sample time.
An explicitly empty keyframe is a zero matrix. With no earlier/later
available keyframe, hold the first/last matrix. Correction timeline behavior
across tiles is defined in Section 11.3.

## 11. Reconstruction semantics

### 11.1 Geometry and observation matrix

For object position `(x,y,z)`, define:

```text
u = clamp((x+1)/2, 0, 1)
v = clamp(1-y,     0, 1)
h = clamp(z,       0, 1)
P(a) = [cos(pi*a/2), sin(pi*a/2)]
```

The front vector is in `L,R,C` order:

```text
F(u) = [cos(pi*u), 0, sin(pi*u)]                    if u <= 1/2
F(u) = [0, sin(pi*(u-1/2)), cos(pi*(u-1/2))]         otherwise
```

For panner version 1, `theta = h*pi/2`. For version 2,
`theta = h*acos(rho)`. Form:

```text
floor = [cos(pi*v/2)*F(u), sin(pi*v/2)*P(u)]
ground = cos(theta)*floor
height = sin(theta)*P(u)
geometric_column = [ground, height]
observation_column = [ground + A*height, height]
```

Negative y values give `v=1`, the rearward limit of this coding panner.
Target renderers select their own panning laws.

Evaluate the geometric panner on the global source-sample grid at multiples
of 240, with additional control points at trajectory knots. Between adjacent
control points, interpolate the seven **gains** linearly. Do not renormalize
the interpolated gain column. Apply the fixed linear fold to that column.
At an STFT center, use the resulting column for each object to form the real
`7 x N` observation matrix `D`.

The encoder MUST form the transported audio from these reconstructed
trajectories and gains. A common `headroom_gain` multiplies every transported
signal, including auxiliary observations and LFE. The source objects already
include authored source gain and activity.

### 11.2 Anchor covariance

For band `b`, find the maximum active log power
`m = max_i(0.5*ell[i][b])`. Inactive objects have exactly zero power.
For active objects, calculate:

```text
p[i] = 10^(max(0.5*ell[i][b] - m, -90)/10)
q[i] = p[i] / sum_active(p)
```

If no object is active, every `q[i]` is zero. A common offset in all active
log powers cancels. An active object's relative level is bounded to 90 dB
below the largest active value before normalization.

Set `C[i,i]=q[i]`. For a present pair `(a,b)`, set
`C[a,b] = sqrt(q[a]*q[b])*rho_ab` and `C[b,a]=conj(C[a,b])`.
All other off-diagonal entries are zero. Absent or zero-magnitude coefficients
contribute zero. Disjoint pairs and magnitudes at most 15/16 make this matrix
positive semidefinite.

### 11.3 Parameter timelines

Install all covariance anchors and correction keyframes of a successfully
decoded tile before processing that tile's available audio. Keep the timeline
across tile boundaries and ordinary checkpoints. A new anchor at an existing
sample replaces the old entry. Processing follows packet/tile arrival order;
a decoder MUST NOT retroactively reprocess a hop when a later tile arrives.

At hop center `t`, select the nearest installed covariance anchors on either
side. Before the first or after the last, hold that endpoint. Between samples
`t0` and `t1`, let `a=(t-t0)/(t1-t0)` and calculate:

```text
C(t)  = (1-a)*C0  + a*C1
nu(t) = (1-a)*nu0 + a*nu1
```

Interpolate the full covariance entries, including the union of pair terms
from both anchors. Different pair lists can give one object two interpolated
off-diagonal terms.

For each band, OR the unobservable flags of all seven observations in both
selected anchors. If any is true, the reconstructed object spectra for that
band are zero. This OR includes both anchors even when `a=0`.

Correction keyframes form a separate persistent timeline. Interpolate dense
`delta_M` matrices using Section 10.6 and the keyframes available when the hop
is processed. An uncorrected tile adds no new keyframes. It does not clear
the previous correction; a zero keyframe is needed to define a fade to zero.
Checkpoint installation does not restore or clear correction history.

### 11.4 Baseline reconstruction matrix

For each observable band, use its interpolated `C`, `nu`, and current `D`:

$$
Z=DCD^H,\qquad
K_0=Z+\operatorname{tr}(Z)\operatorname{diag}(\nu).
$$

On channels where `K0[j,j]>0`, set `S[j,j]=1/sqrt(K0[j,j])`. Channels with
zero diagonal have scale zero and contribute no estimate. Let
`R=S*K0*S` on the supported channels. The baseline matrix is:

$$
M_0=CD^H S(R+10^{-12}I)^{-1}S.
$$

The floor is dimensionless and applies after diagonal normalization.
Unsupported channels use an identity block with zero scales. The implementation
uses a Hermitian Cholesky solve; an equivalent solve is permitted.

For full enhanced reconstruction, add the interpolated correction:
`M=M0+delta_M`. For baseline-only reconstruction, use `M=M0`.
For each positive-frequency bin in the band, reconstruct `s_hat=M*y`.

The correction is additive after the covariance solve. There is no second
per-object activity multiplication on `M`. Consequently, a nonzero correction
row can contribute even when that object's baseline covariance power is zero.
Scene-wide inactivity and unobservable-band muting still suppress the output.

The current numerical zero rules are: if no band's covariance trace exceeds
`1e-30`, emit zero object spectra for the hop; if a band's unweighted decoded
core spectral energy is at most `1e-24`, emit zero for that band. A non-silent,
observable system that cannot be solved produces a reconstruction error.

### 11.5 STFT and real endpoints

The analysis/synthesis window is the periodic square-root Hann:

$$
w[n]=\sqrt{\tfrac12-\tfrac12\cos(2\pi n/1920)},\quad 0\le n<1920.
$$

With programme source origin `s0`, hop `h` is centered at `s0+240*h`.
Analysis uses an unnormalized forward transform:

$$
Y(h,k)=\sum_{n=0}^{1919} w[n]\,
y[s_0+240h+n-960]e^{-j2\pi kn/1920}.
$$

Samples before the programme and beyond its aligned audio are zero.
Tile boundaries do not reset the analysis window. Flush the analysis far
enough to include every hop window that contributes to the retained output.

At DC and Nyquist, solve the baseline from `Re(C)` before applying it.
Add `Re(delta_M)` at those bins and force the output imaginary part to zero.
Interior positive bins use the complex matrix; negative bins use conjugate
symmetry.

The inverse transform includes the `1/1920` normalization. For an inverse
frame `v_h` and local index `m_h=n-240h+960`, output:

$$
\hat s[n]=\frac{\sum_h w[m_h]v_h[m_h]}
{\sum_h w[m_h]^2}.
$$

Only windows covering sample `n` contribute. If the denominator is at most
`1e-24`, output zero. This normalization also applies at programme boundaries.

### 11.6 LFE, output level, and object metadata

LFE bypasses the object covariance and correction. It uses the same aligned
sample clock and analysis/synthesis path in the reference decoder. After
synthesis, divide both object and LFE PCM by `headroom_gain` once. Stock Opus
playback retains the transported signal level.

The object output consists of LFE followed by the `N` ordered object signals.
Object IDs are `index+1`. Output object gain is unity and positions follow
the decoded trajectories. Output metadata MUST NOT apply an additional
authored activity gate to the synthesized overlap tail. The downstream
speaker/binaural renderer and its output layout are outside the bitstream
definition.

## 12. State transitions and error handling

### 12.1 Decode procedure

For each tile:

1. Recover complete Opus packets from the container.
2. Split the multistream packet using the public codec binding.
3. Extract Salsa fragments from the mono center padding.
4. Reassemble the complete record bundle.
5. Validate any configuration against `OpusHead` and the supported profile.
6. Install a matching valid checkpoint, if present.
7. Decode the main parameter block and any present correction into temporary
   state.
8. Verify lengths, ranges, references, and rANS termination.
9. Commit the parameter state and install the tile's timelines.
10. Decode and align the audio observations and LFE.
11. Run Section 11 reconstruction and emit the retained source interval.

Audio decoding can be scheduled earlier if its order and state remain
unchanged. Invalid metadata MUST NOT partially update the committed
parameter predictor or its probability models.

### 12.2 Missing or damaged enhancement

A missing tile payload, invalid parameter stream, or unsupported required
model prevents enhanced reconstruction for that interval. A playback adapter
SHOULD continue the valid public fallback audio where available. It MUST NOT
label guessed parameters as a valid enhanced decode.

After a missing or invalid predicted tile, the decoder MUST treat the
parameter predictor chain as unavailable. Later delta tiles cannot repair
that chain. A valid matching checkpoint followed by a valid tile restores
enhanced reconstruction. A failed checkpoint installation leaves the prior
committed state unchanged.

This rule applies to enhancement recovery. It does not promise recovery from
every malformed Ogg page, Opus packet, or record wrapper. The current parser
can return a fatal transport error before a tile reaches the DSP.

### 12.3 Reset and random access

A parameter checkpoint MUST NOT reset continuous Opus decoder state or STFT
overlap during sequential playback. A full seek/discontinuity reset does
clear audio, parameter, and timeline state. After a reset, a decoder needs a
known configuration, an independent checkpoint, and sufficient audio preroll.

Checkpoint cadence is not encoded as a field. The reference encoder places
one at the first tile and every 48,000 source samples relative to programme
start. Other cadences do not change the parameter syntax.

Corrections and preceding covariance anchors are not stored in checkpoints.
Random-access output near the entry boundary can therefore differ from
continuous decoding even after the integer predictors are restored. An
adapter MUST allow preroll and discard its output before the requested
presentation start.

### 12.4 Validation boundaries

Before accepting a complete enhancement, a reader MUST check the following:

- All nested byte lengths fit their enclosing payload and declared limits.
- Fragment indices, counts, associations, and reconstructed total agree.
- Known versions, required algorithms, and channel bindings are supported.
- Object-index references resolve and required arrays have the scene count.
- Main-tile pairs are disjoint and coefficient codes are in range.
- Timestamp arithmetic, power arithmetic, and allocation sizes do not overflow.
- Correction support is ordered and within the scene; scales are finite.
- Every entropy block terminates at its marker with no remaining words.
- The enhancement contains only its main block and optional correction trailer.

Ogg CRC protects page bytes. Salsa adds no record CRC, authentication tag,
or separate checksum. The rANS marker detects termination errors but MUST
NOT be treated as cryptographic or complete corruption detection.

## Annex A. Reference implementation (informative)

### A.1 Encoder choices

The encoder uses restricted-low-delay Opus at 48 kHz, complexity 10, with
20-ms packets. These settings explain the tested 120-sample lookahead; they
are not additional fields in `SCFG`. The five active streams in
`MultistreamCodec` use CBR. The shared silence stream uses unconstrained VBR.
The legacy `CoreCodec` has a different LFE VBR policy.

The CLI's default targets are 146, 80, 88, 70, and 24 kbps for front,
surround, auxiliary, center, and LFE. `SalsaSpatialStreamConfig::default()`
instead initializes 200, 128, 120, 112, and 24 kbps. The configuration records
the values selected by the caller. Neither list is a decoder requirement.

The CLI defaults to `beta=7.5e-7`, correction rank 4, support 16, three
keyframes per tile, and correction rate weight 0.6. The syntax permits rank
7, support 32, and eight keyframes. Power search tests 0.5, 1, 2, and 4 dB
quantizers on the fixed half-dB predictor grid. Those choices have no
quantizer-selector field.

For a full tile, regular anchors occur at offsets
`0,960,1920,2880,3840,4560`. Search can add up to four transient anchors on
the 240-sample grid. The trajectory fitter uses a maximum moving-knot gap
of 4,800 samples and a gain-vector error target of 0.005. These are encoder
search policies, not hidden parser assumptions.

The writer emits one fragment on the first packet of each tile. It resends
configuration at checkpoint tiles once at least 48,000 samples have elapsed
since the previous configuration. The first configuration is full. Later
ones omit the initial scene only when every checkpoint trajectory is nonempty.

### A.2 Parser behavior and implementation limits

The following table describes validation and processing in the current code.

| Area | Current implementation |
|---|---|
| Reserved values | Fragment wrapper bytes and record flags are read but not validated. Configuration `profile` is stored without dispatch. |
| Fragment lengths | `Reassembler` concatenates fragments and truncates to the declared total; it does not reject every sum mismatch. Fragment parsing does not require complete input consumption. |
| Record association | The fragment key is used for reassembly, but `MultistreamReader` does not compare every inner ID/timestamp with that key. Duplicate known records replace earlier pending values. |
| Configuration validation | Descriptor validation checks permutations, auxiliary ownership, references, and finite fold values. It does not enforce every profile-specific stage order, preset value, initial-scene count, or object metadata restriction. |
| General descriptors | The DSP uses seven fixed panner rows and the first stage's routing. Serialized extra stages and residual-domain descriptors do not implement a general staged decoder. |
| Trailing bytes | `STIL` rejects a suffix. Configuration parsing and the entropy wrapper do not reject every unused byte suffix. rANS `finish()` validates the coded words, not the enclosing slice length. |
| Main parameter validation | The reader bounds allocations and arithmetic, but does not require sorted in-tile anchor times or enforce all canonical encoder choices. Error values and trajectory codes are clamped as specified. |
| Checkpoint validation | Installation checks array lengths, object count, and activity consistency. The checkpoint reader does not fully validate pair disjointness and indices before conversion. |
| Correction support | The syntax reader checks indices against 128, not the configured `N`; dense expansion ignores indices outside the scene. Some scalar helper reads use `expect`, so malformed correction data can panic rather than return an error. |
| Correction activity | The encoder selects support from active fitting regions. The DSP does not separately gate each corrected row by activity. |
| Random access state | A checkpoint restores parameter predictors, not covariance/correction timeline history or audio state. |
| Metadata fallback | DSP fallback puts the first `min(N,7)` observation signals into object slots and leaves later slots silent. It retains object trajectories and is not a public 5.1 speaker-bed output API. Its 960-sample mode transition is evaluated at hop centers. |
| Object metadata | The DSP generates IDs `1..N`, unity gain, and position events. Arbitrary serialized initial IDs, size, spread, gain, and enabled values are not preserved as output controls. |
| Pre-skip | The core decoder queries the tested encoder lookahead. The shared parser does not pass arbitrary `OpusHead.pre_skip` or output gain into the DSP configuration. |
| Ogg paging | The writer emits one page per packet and casts the lacing count to `u8`; it does not split oversized packets across pages. The GStreamer tile scanner counts five audio pages and therefore assumes the reference page layout. |
| Packet bounds | The framing reader bounds aggregate packet bytes by `stream_count * 61,200`. The writer also bounds the metadata-bearing center packet to 61,200 bytes; this is separate from Opus's per-audio-frame bound. |
| Container scope | The Ogg reader validates CRC and continuation, retains one serial, and does not support chained serials. It does not validate every Ogg version/sequence condition. |

### A.3 Final duration and drain handling

`OggSalsaWriter::write_frame` advances the granule by 960 for each audio
packet, including a padded final partial packet. `write_tail` emits an EOS
audio packet with granule `pre_skip + total_samples`, where `total_samples`
is that packet-rounded count. It therefore trims the extra drain packet but
does not trim the fractional-packet source padding. The private `STIL.frames`
field still records the source duration.

`SalsaSpatialParser` currently discards each demuxed packet's granule and EOS
metadata before passing it to `MultistreamReader`. The reader groups packets
in fives. A final drain packet can therefore be grouped with a partial tile
or emitted as a separate fallback tile. The current generic path is not a
complete implementation of sample-exact tail handling for all programme
lengths. These limits must be considered when comparing container playback
duration with the sum of private tile durations.

### A.4 Recovery integration

`SceneParameterDecoder::decode_tile` clones predictor and context state, then
commits only after the main stream and correction decode succeed.
`install_checkpoint` also validates on a temporary predictor. The DSP keeps
playing its fallback mode until a complete checkpoint and valid tile restore
the chain. Its crossfade is 960 source samples, evaluated on the hop grid.

The GStreamer parser indexes checkpoint byte offsets and translates flushing
time seeks to a preceding checkpoint with preroll. The adapter retains the
requested presentation start for output clipping. This behavior is an
integration policy; it does not add a seek table to the file.

## Annex B. Encoding examples (informative)

### B.1 Identification packet

For the initial mapping and pre-skip 120, the complete `OpusHead` packet is:

```text
4f 70 75 73 48 65 61 64 01 08 78 00 80 bb 00 00
00 00 01 06 03 00 06 01 02 03 08 08 07
```

This is the packet payload, not an Ogg page. An Ogg page adds its own header,
lacing table, serial, sequence number, and checksum.

### B.2 Position and integer examples

For coordinate codes `(0,4095,4095)`, the position is `(-1,1,1)`.
For x code 2048, the coordinate is `1/4095`, approximately `0.0002442002442`.

For unsigned binary tails:

| Integer | Logical binary symbols in `B` |
|---:|---|
| 0 | `0` |
| 1 | `10` |
| 2 | `1100` |
| 3 | `1101` |
| 5 | `111001` |

These are adaptive categorical symbols, not bits copied directly to the
file. For `S`, values `0,-1,1,-2,15,-16,16` map to zigzag values
`0,1,2,3,30,31,32`; the last emits escape symbol 32 followed by `B(tail)=0`.

### B.3 Layer sizes

A `STIL` record with no enhancement has 32 payload bytes and a 12-byte
`SLEN` header. As the sole record in one fragment, it makes a 44-byte bundle
and a 74-byte `SLSR` fragment. With no saved original Opus padding, the
`SALS` wrapper adds 17 bytes, giving 91 wrapper bytes. The experimental
extension adds `F1 5B`, giving 93 padding bytes. The surrounding Opus packet
still needs its own TOC, frame-count byte, padding-length header, audio
payload, and, in multistream form, a self-delimited frame size.

An empty enhancement in this example requests fallback.

### B.4 Entropy byte vector

Start one binary context with counts `[1,1]`. Encode the logical symbol
sequence `[0,1,0,0,1]` using that context, including the count update after
each symbol. The complete entropy block is:

```text
01 00 00 00 39 33 33 25
```

The first four bytes specify one word. The word is `0x25333339`. Decoding
the five symbols and the fixed termination marker leaves state zero and
an empty word stack. This vector includes the entropy framing only.

