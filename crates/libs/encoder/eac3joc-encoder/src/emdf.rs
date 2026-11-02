// SPDX-License-Identifier: MPL-2.0
//! EMDF container, OAMD and program-information writer.
//!
//! Bit layout follows ETSI TS 103 420 and ETSI TS 102 366 Annex H. The
//! container's protection field is written as a public placeholder before
//! injection; a configured Dolby key then signs the complete E-AC-3 frame.

use crate::bitwriter::{BitReader, BitWriter};

/// EMDF marker (byte-aligned syncword).
const EMDF_SYNC: u16 = 0x5838;
/// EMDF payload id for Object Audio Metadata.
pub const PAYLOAD_OAMD: u8 = 11;
/// EMDF payload id for Joint Object Coding.
pub const PAYLOAD_JOC: u8 = 14;

/// (x: −1 left → +1 right, y: −1 back → +1 front, z: 0 floor → 1 ceiling).
#[derive(Debug, Clone, Copy)]
pub struct ObjectPos {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Quantize DAMF coordinates to OAMD position codes.
/// OAMD: X 0=left..1=right (×1/62), Y 0=front..1=back (×1/62), Z 0=floor..1=ceiling (×1/15).
fn quantize_pos(p: ObjectPos) -> (u32, u32, u32, u32) {
    let oamd_x = ((p.x + 1.0) * 0.5).clamp(0.0, 1.0);
    let oamd_y = ((1.0 - p.y) * 0.5).clamp(0.0, 1.0);
    let oamd_z = p.z.clamp(0.0, 1.0);
    let px = (oamd_x * 62.0).round() as u32; // 0..62
    let py = (oamd_y * 62.0).round() as u32;
    let pz = (oamd_z * 15.0).round() as u32; // 0..15
    (px, py, 1, pz) // z sign always positive (z >= 0)
}

/// Encode an OAMD payload for a "dynamic object-only + LFE bed" program:
/// object 0 is the LFE bed, objects 1.. are the dynamic objects. Single info block at frame start.
/// Returns whole bytes (the EMDF payload body).
pub fn encode_oamd(objects: &[ObjectPos], lfe: bool) -> Vec<u8> {
    let bed_count: usize = if lfe { 1 } else { 0 };
    let object_count = bed_count + objects.len();
    assert!(
        (1..=159).contains(&object_count),
        "object_count out of range"
    );

    let mut w = BitWriter::new();
    // object_audio_metadata_payload()
    w.write(0, 2); // oa_md_version_bits
    write_count5(&mut w, (object_count - 1) as u32); // object_count_bits (=count-1)
    // program_assignment(): dynamic-object-only + LFE
    w.bit(1); // b_dyn_object_only_program
    w.bit(if lfe { 1 } else { 0 }); // b_lfe_present
    w.bit(0); // b_alternate_object_data_present
    w.write(2, 4); // oa_element_count_bits: object_element + trim_element

    // oa_element_md()
    w.write(1, 4); // oa_element_id_idx = 1 (object element)
    let body = encode_object_element(objects, bed_count, object_count);
    // TS 103 420 §5.6.4.3: oa_element_size is the total length in BYTES of
    // b_discard_unknown_element + oa_element + padding (no alternate id here).
    let element_bits = 1 + body.bit_len;
    let element_bytes = element_bits.div_ceil(8);
    let size_value = (element_bytes - 1) as u32;
    write_var_max4(&mut w, size_value);
    // (b_alternate_object_data_present == 0 → no alternate id)
    w.bit(0); // b_discard_unknown_element
    w.copy_bits(&body.bytes, body.bit_len); // oa_element() = object_element()
    for _ in 0..(element_bytes * 8 - element_bits) {
        w.bit(0); // padding, counted by oa_element_size
    }

    // DME appends a trim_element on every frame. The values are the Dolby encoder's defaults for
    // this master: warp_mode=1 (Y x2), default global trim, and no per-object disable because
    // every DAMF element has trimBypass=false. build_timelines() rejects trimBypass=true until
    // the per-object b_disable_trim array is implemented.
    w.write(2, 4); // oa_element_id_idx = 2 (trim_element)
    write_var_max4(&mut w, 0); // oa_element_size = 1 byte
    w.bit(0); // b_discard_unknown_element
    w.write(0b01, 2); // warp_mode = 1 (object_position_Y multiplied by 2)
    w.write(0b00, 2); // reserved
    w.write(0b00, 2); // global_trim_mode = default_trim
    w.bit(0); // b_disable_trim_per_obj = 0

    w.into_bytes()
}

struct Bits {
    bytes: Vec<u8>,
    bit_len: usize,
}

fn encode_object_element(objects: &[ObjectPos], bed_count: usize, object_count: usize) -> Bits {
    let mut w = BitWriter::new();
    // md_update_info()
    w.write(0, 2); // sample_offset_code = 0 (no offset)
    w.write(0, 3); // num_obj_info_blocks_bits = 0 → 1 block
    // block_update_info(0)
    w.write(0, 6); // block_offset_factor_bits = 0 (start of frame)
    w.write(0b10, 2); // ramp_duration_code = 2 → 1536-sample ramp (smooth, full frame)

    w.bit(1); // b_reserved_data_not_present = 1 (no reserved 5 bits)

    // object_data() for each object (single block, blk == 0). Objects [0, bed_count) are beds.
    for obj in 0..object_count {
        let is_bed = obj < bed_count;
        let pos = if is_bed {
            None
        } else {
            objects.get(obj - bed_count).copied()
        };
        encode_object_info_block(&mut w, is_bed, pos);
    }

    let bit_len = w.bit_len();
    Bits {
        bytes: w.into_bytes(),
        bit_len,
    }
}

fn encode_object_info_block(w: &mut BitWriter, is_bed: bool, pos: Option<ObjectPos>) {
    w.bit(0); // b_object_not_active = 0 (active)
    // blk == 0 ⇒ object_basic_info_status_idx = 0b01 (implicit) ⇒ object_basic_info()
    //   object_basic_info[] = {true, true}
    // object_gain_idx: real Dolby files use 0b00 (unity) for the LFE bed but 0b11 ("reuse last
    // gain" → no per-frame attenuation) for dynamic objects. Match that exactly.
    w.write(if is_bed { 0b00 } else { 0b11 }, 2);
    w.bit(1); // b_default_object_priority = 1 (no priority bits)

    if !is_bed {
        // blk == 0, not bed ⇒ object_render_info_status_idx = 0b01 ⇒ object_render_info()
        //   obj_render_info[] = {true, true, true, true}
        let p = pos.unwrap_or(ObjectPos {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });
        let (px, py, zsign, pz) = quantize_pos(p);
        // obj_render_info[0]: position (blk==0 ⇒ no differential bit)
        w.write(px, 6); // pos3D_X_bits
        w.write(py, 6); // pos3D_Y_bits
        w.write(zsign, 1); // pos3D_Z_sign_bits
        w.write(pz, 4); // pos3D_Z_bits
        w.bit(0); // b_object_distance_specified = 0
        // obj_render_info[1]: zone_constraints_idx(3) + b_enable_elevation(1). Every real Dolby file
        // sets this field to 0b0001 (elevation ENABLED); we previously wrote 0b0000, which Cavern
        // ignores but a conformant Dolby renderer may read as "height disabled" → flattened objects.
        w.write(0b0001, 4);
        // obj_render_info[2]: object_size_idx = 0b00 (point source)
        w.write(0b00, 2);
        // obj_render_info[3]: b_object_use_screen_ref = 0
        w.bit(0);
        // b_object_snap = 0
        w.bit(0);
    }

    w.bit(0); // b_additional_table_data_exists = 0
}

/// object_count_bits: 5 bits, with a 7-bit extension when == 0x1F.
fn write_count5(w: &mut BitWriter, count_minus_1: u32) {
    if count_minus_1 < 0x1F {
        w.write(count_minus_1, 5);
    } else {
        w.write(0x1F, 5);
        w.write(count_minus_1 - 0x1F, 7);
    }
}

/// variable_bits_max(4, 4) for oa_element_size_bits.
fn write_var_max4(w: &mut BitWriter, value: u32) {
    w.write_var(value, 4);
}

/// Encode the 3-byte programme_information() payload (TS 102 366 H.3.2) we carry next to OAMD
/// and JOC. `ph90filt` must match the core: set it when `downmix --surround-phase-shift` applied
/// the +90° Ls/Rs all-pass. The DRC profile fields are informational; they describe the profile
/// the core encoder would have used for `compr`/`dynrng` data.
pub fn encode_program_info(ph90filt: bool, dynrngprof: u8, comprprof: u8) -> Vec<u8> {
    assert!(dynrngprof < 8 && comprprof < 8, "DRC profile is 3 bits");
    let mut w = BitWriter::new();
    w.write(0, 2); // version = 0
    w.bit(0); // activechane = 0
    w.bit(0); // dmixtype = 0
    w.bit(0); // upmixtype = 0
    w.bit(1); // preproinfoe = 1
    w.bit(0); // suratten = 0
    w.bit(ph90filt as u32); // ph90filt: +90° Ls/Rs all-pass applied
    w.bit(1); // lfefilt = 1
    w.write(0, 2); // lfemonlevcod = 0 (not indicated)
    w.bit(1); // drcprofinfoe = 1
    w.write(dynrngprof as u32, 3);
    w.write(comprprof as u32, 3);
    w.bit(0); // specprocinfoe = 0
    w.bit(0); // chancplinfoe = 0
    w.bit(0); // enhncrnge = 0
    w.into_bytes()
}
// --------------------------------------------------------------------------- EMDF protection seam
//
// `emdf_protection` (ETSI TS 102 366 v1.4.1 Annex H.2.1.4 / H.2.2) is a KEYED authentication code,
// not a plain checksum. The spec states `key_id` selects an "authentication key" and that the
// calculation of `protection_bits_primary` / `protection_bits_secondary` is "implementation
// dependent and is not defined in the present document." No open decoder (ffmpeg, Cavern, truehdd)
// computes or validates it; Dolby-certified hardware does. We proved empirically that no public CRC
// reproduces Dolby's values (see `verify_emdf_protection` + the brute-force documented in README).
//
// The real protection is computed after injection by `sign_frame_with_key` below, because it
// authenticates the complete E-AC-3 frame, not just the container. The `EmdfProtector` trait remains
// the container-level placeholder used while building the EMDF; a configured `KeyedProtector` also
// installs the post-injection frame signer, which overwrites both fields in the final frame.

/// Protection-length selector → field width in bits. Index 0 ("reserved" for primary) treated as 0.
pub const PROT_LEN: [u32; 4] = [0, 8, 32, 128];

/// The seam where the (proprietary, keyed) `emdf_protection` computation lives.
pub trait EmdfProtector: Send + Sync {
    /// 3-bit `key_id` written into the EMDF header — which authentication key signs this stream.
    fn key_id(&self) -> u8 {
        0
    }
    /// (primary, secondary) 2-bit length selectors. Real Dolby DD+ JOC uses (2, 1) = 32-bit primary
    /// + 8-bit secondary, which we mirror by default.
    fn length_codes(&self) -> (u8, u8) {
        (2, 1)
    }
    /// Primary protection value over `covered` = first `nbits` body bits (emdf_version .. end of the
    /// 2+2 length-selector bits), MSB-first byte-padded.
    fn primary(&self, covered: &[u8], nbits: usize) -> u128;
    /// Secondary protection value over `covered` = the same region PLUS the primary bits.
    fn secondary(&self, covered: &[u8], nbits: usize) -> u128;
}

/// Default protector: public, deterministic CRCs — NOT Dolby's keyed MAC. The emitted field is
/// well-formed and round-trips through every software oracle that ignores protection (ffmpeg,
/// Cavern), but it is *unsigned*: Dolby-certified hardware that validates the real signature rejects
/// it. This is the honest default — structurally valid, cryptographically absent.
pub struct PublicCrcProtector;

impl EmdfProtector for PublicCrcProtector {
    fn primary(&self, covered: &[u8], nbits: usize) -> u128 {
        crc32_bits(covered, nbits) as u128
    }
    fn secondary(&self, covered: &[u8], nbits: usize) -> u128 {
        crc8_bits(covered, nbits) as u128
    }
}

/// Keyed protector. The container-level methods write a deterministic placeholder and the
/// `key_id`/length fields; [`KeyedProtector::sign_frame`] computes the real primary/secondary over
/// the complete injected frame. Selected from the CLI (`--emdf-key` / `DOLBY_EMDF_KEY`).
pub struct KeyedProtector {
    pub key: Vec<u8>,
    pub key_id: u8,
}

impl EmdfProtector for KeyedProtector {
    fn key_id(&self) -> u8 {
        self.key_id
    }
    fn primary(&self, covered: &[u8], nbits: usize) -> u128 {
        dolby_keyed_mac(&self.key, self.key_id, covered, nbits, 32)
    }
    fn secondary(&self, covered: &[u8], nbits: usize) -> u128 {
        dolby_keyed_mac(&self.key, self.key_id, covered, nbits, 8)
    }
}

impl KeyedProtector {
    /// Sign one complete E-AC-3 frame after EMDF injection.
    ///
    /// `emdf_start_bit` is the bit offset of the EMDF sync inside `frame`; `emdf` is the same
    /// container byte-aligned, and `excluded` lists the bit ranges (start inclusive, end
    /// exclusive) that are not authenticated (frame header, info_metadata, addbsi, skip fields,
    /// and the frame tail). The rule is the one used by Dolby streams:
    ///
    /// `primary = HMAC-SHA256(key, authenticated_core || blanked_container)[0..4]`
    ///
    /// `secondary = HMAC-SHA256(key, full_primary_digest)[0]`
    ///
    /// The container's 40 protection bits are zeroed before hashing. The frame CRC2 is recomputed
    /// afterwards.
    pub fn sign_frame(
        &self,
        frame: &mut [u8],
        emdf_start_bit: usize,
        emdf: &[u8],
        excluded: &[(usize, usize)],
    ) -> Result<(), String> {
        sign_frame_with_key(
            frame,
            emdf_start_bit,
            emdf,
            excluded,
            &self.key,
            self.key_id,
        )
    }
}

fn merge_excluded(ranges: &[(usize, usize)], total: usize) -> Vec<(usize, usize)> {
    let mut r: Vec<(usize, usize)> = ranges
        .iter()
        .copied()
        .filter(|&(a, b)| a < b && a < total)
        .map(|(a, b)| (a, b.min(total)))
        .collect();
    r.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(r.len());
    for (a, b) in r {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

#[inline]
fn frame_bit(frame: &[u8], bit: usize) -> u32 {
    u32::from((frame[bit >> 3] >> (7 - (bit & 7))) & 1)
}

#[inline]
fn set_frame_bit(frame: &mut [u8], bit: usize, value: u32) {
    let mask = 1u8 << (7 - (bit & 7));
    if value & 1 == 1 {
        frame[bit >> 3] |= mask;
    } else {
        frame[bit >> 3] &= !mask;
    }
}

fn zero_frame_bits(frame: &mut [u8], bit: usize, n: usize) {
    for p in bit..bit + n {
        set_frame_bit(frame, p, 0);
    }
}

/// The authenticated core of a frame: every bit not covered by `excluded`, packed MSB-first and
/// padded to the nearest 16-bit word (halves up), matching the Dolby protection rule.
pub fn authenticated_core(frame: &[u8], excluded: &[(usize, usize)]) -> Vec<u8> {
    let total = frame.len() * 8;
    let ranges = merge_excluded(excluded, total);
    let mut w = BitWriter::new();
    let mut next = 0usize;
    for (a, b) in ranges {
        if a > next {
            for p in next..a {
                w.bit(frame_bit(frame, p));
            }
        }
        next = next.max(b);
    }
    if next < total {
        for p in next..total {
            w.bit(frame_bit(frame, p));
        }
    }
    let kept = w.bit_len();
    let mut out = w.into_bytes();
    out.resize((kept + 8) / 16 * 2, 0);
    out
}

/// Sign one complete E-AC-3 frame with the full EMDF protection rule.
pub fn sign_frame_with_key(
    frame: &mut [u8],
    emdf_start_bit: usize,
    emdf: &[u8],
    excluded: &[(usize, usize)],
    key: &[u8],
    key_id: u8,
) -> Result<(), String> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    if key.is_empty() {
        return Err("empty EMDF key".into());
    }
    if emdf.len() < 4 {
        return Err("short EMDF container".into());
    }
    let length_bytes = (usize::from(emdf[2]) << 8) | usize::from(emdf[3]);
    if emdf.len() < 4 + length_bytes {
        return Err("truncated EMDF container body".into());
    }
    let offsets = field_offsets(emdf).ok_or("malformed EMDF container")?;
    if offsets.prim_bits != 32 || offsets.sec_bits != 8 {
        return Err(format!(
            "unsupported protection lengths: primary={} secondary={}",
            offsets.prim_bits, offsets.sec_bits
        ));
    }
    let stored_key_id = (0..3).fold(0u8, |v, i| {
        (v << 1) | frame_bit(emdf, offsets.key_id_bit + i) as u8
    });
    if stored_key_id != key_id & 7 {
        return Err(format!(
            "EMDF key_id {stored_key_id} does not match signing key_id {}",
            key_id & 7
        ));
    }
    let body_off = offsets
        .protection_start
        .checked_sub(32)
        .ok_or("protection starts before the EMDF body")?;
    if body_off + 40 > length_bytes * 8 {
        return Err("protection bits lie outside the EMDF body".into());
    }
    let emdf_end = emdf_start_bit + emdf.len() * 8;
    if !excluded
        .iter()
        .any(|&(a, b)| a <= emdf_start_bit && b >= emdf_end)
    {
        return Err("EMDF carrier is not excluded from the authenticated core".into());
    }

    let core = authenticated_core(frame, excluded);
    let mut body = emdf[4..4 + length_bytes].to_vec();
    zero_frame_bits(&mut body, body_off, 40);

    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(&core);
    mac.update(&body);
    let primary = mac.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(&primary);
    let secondary = mac.finalize().into_bytes();

    let protection = emdf_start_bit + offsets.protection_start;
    for i in 0..32 {
        set_frame_bit(
            frame,
            protection + i,
            u32::from((primary[i >> 3] >> (7 - (i & 7))) & 1),
        );
    }
    for i in 0..8 {
        set_frame_bit(
            frame,
            protection + 32 + i,
            u32::from((secondary[0] >> (7 - i)) & 1),
        );
    }

    let n = frame.len();
    if n < 4 {
        return Err("short E-AC-3 frame".into());
    }
    // E-AC-3 does not invert encinfo/crcrsv when CRC2 equals the sync word (unlike AC-3).
    let crc = reac3e::crc16(&frame[2..n - 2]);
    frame[n - 2] = (crc >> 8) as u8;
    frame[n - 1] = crc as u8;
    Ok(())
}

/// Container-level placeholder for the pre-injection EMDF body. This is not the Dolby frame
/// protection; `sign_frame_with_key` computes that over the complete injected frame. Kept so
/// unsigned and diagnostic paths still emit a deterministic, structurally valid field.
fn dolby_keyed_mac(key: &[u8], key_id: u8, covered: &[u8], nbits: usize, out_bits: u32) -> u128 {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&[key_id]);
    let nbytes = nbits.div_ceil(8).min(covered.len());
    mac.update(&covered[..nbytes]);
    let tag = mac.finalize().into_bytes();
    let mut v: u128 = 0;
    for i in 0..out_bits as usize {
        v = (v << 1) | ((tag[i >> 3] >> (7 - (i & 7))) & 1) as u128;
    }
    v
}

/// Append a protection value of `n` bits (MSB-first) to the body writer.
fn write_protection_bits(b: &mut BitWriter, val: u128, n: u32) {
    for i in (0..n).rev() {
        b.bit(((val >> i) & 1) as u32);
    }
}

// --------------------------------------------------------------------------- EMDF container

/// Wrap one or more (payload_id, payload_bytes) into an EMDF container, with the payload-config
/// values mandated by ETSI TS 103 420 §8.2 Table 56 (duratione=0, groupide=1, codecdatae=0,
/// discard_unknown=0, frame-aligned, priority/proc_allowed=0). Returns whole bytes starting 0x58 0x38.
/// Wrap payloads into an EMDF container with an explicit protector.
pub fn wrap_emdf_with(payloads: &[(u8, Vec<u8>)], prot: &dyn EmdfProtector) -> Vec<u8> {
    // Body after the 16-bit length field.
    let mut b = BitWriter::new();
    b.write(0, 2); // emdf_version = 0
    b.write(prot.key_id() as u32, 3); // key_id (selects the authentication key)
    for (id, payload) in payloads {
        b.write(*id as u32, 5); // emdf_payload_id
        b.bit(0); // smploffste = 0
        b.bit(0); // duratione = 0
        b.bit(1); // groupide = 1
        b.write_var(0, 2); // groupid = 0
        b.bit(0); // codecdatae = 0 (matches real Dolby JOC streams)
        b.bit(0); // discard_unknown_payload = 0 → enter config block
        b.bit(1); // payload_frame_aligned = 1 (smploffste == 0)
        b.write(0, 2); // create_duplicate = 0, remove_duplicate = 0
        b.write(0, 7); // priority + proc_allowed = 0
        b.write_var(payload.len() as u32, 8); // emdf_payload_size (bytes)
        b.copy_bits(payload, payload.len() * 8);
    }
    b.write(0, 5); // emdf_payload_id = 0 → terminate
    // emdf_protection() — ETSI TS 102 366 H.2.1.4. Field widths and the signature itself come from
    // the active EmdfProtector: the public-CRC placeholder by default, or a keyed signer when a key
    // is supplied (--emdf-key / DOLBY_EMDF_KEY). See the "EMDF protection seam" above for why a
    // conformant signature is a proprietary keyed MAC we cannot reproduce open-source.
    let (pc, sc) = prot.length_codes();
    b.write(pc as u32, 2); // protection_length_primary
    b.write(sc as u32, 2); // protection_length_secondary
    let (snap, nbits) = b.snapshot();
    let prim = prot.primary(&snap, nbits);
    write_protection_bits(&mut b, prim, PROT_LEN[pc as usize]); // protection_bits_primary
    let (snap, nbits) = b.snapshot();
    let sec = prot.secondary(&snap, nbits);
    write_protection_bits(&mut b, sec, PROT_LEN[sc as usize]); // protection_bits_secondary
    let body = b.into_bytes();

    let mut w = BitWriter::new();
    w.write(EMDF_SYNC as u32, 16);
    w.write(body.len() as u32, 16); // emdf length in bytes (after this field)
    w.copy_bits(&body, body.len() * 8);
    w.into_bytes()
}

/// CRC-32 (poly 0x04C11DB7, init 0, MSB-first) over the first `nbits` bits of `data`.
fn crc32_bits(data: &[u8], nbits: usize) -> u32 {
    let mut crc: u32 = 0;
    for i in 0..nbits {
        let b = ((data[i >> 3] >> (7 - (i & 7))) & 1) as u32;
        let top = (crc >> 31) & 1;
        crc <<= 1;
        if b ^ top != 0 {
            crc ^= 0x04C1_1DB7;
        }
    }
    crc
}

/// CRC-8 (poly 0x2F, init 0, MSB-first) over the first `nbits` bits of `data`. Used for the EMDF
/// protection field — deterministic and well-formed; the spec leaves the exact algorithm undefined.
fn crc8_bits(data: &[u8], nbits: usize) -> u8 {
    let mut crc: u8 = 0;
    for i in 0..nbits {
        let b = (data[i >> 3] >> (7 - (i & 7))) & 1;
        let top = (crc >> 7) & 1;
        crc <<= 1;
        if b ^ top != 0 {
            crc ^= 0x2F;
        }
    }
    crc
}

/// Bit offsets of the mutable fields inside a byte-aligned EMDF container (one that starts with the
/// `0x5838` sync at bit 0 of `aligned`). All offsets are relative to that sync. Used by the
/// `emdfmutate` probe to rewrite `key_id` / flip `protection_bits` in a genuine stream without
/// disturbing any other bit. The payload walk mirrors `verify_emdf_protection` exactly.
pub(crate) struct EmdfFieldOffsets {
    /// Bit offset of the 3-bit `key_id` field (= 34 for `emdf_version == 0`).
    pub key_id_bit: usize,
    /// Bit offset where `protection_bits_primary` begins.
    pub protection_start: usize,
    /// Length of `protection_bits_primary` in bits (0/8/32/128).
    pub prim_bits: u32,
    /// Length of `protection_bits_secondary` in bits (0/8/32/128).
    pub sec_bits: u32,
}

/// Locate the mutable EMDF fields in `aligned` (which must begin at the `0x5838` sync). Returns
/// `None` if the container is malformed or truncated. Walk kept identical to the one embedded in
/// `verify_emdf_protection` so both agree on `protection_start`.
pub(crate) fn field_offsets(aligned: &[u8]) -> Option<EmdfFieldOffsets> {
    let mut r = BitReader::new(aligned);
    if r.read(16) != 0x5838 {
        return None;
    }
    let emdf_len = r.read(16) as usize;
    let _version = r.read(2);
    let key_id_bit = r.pos; // 34 when version == 0 (the only version genuine streams use)
    let _key = r.read(3);
    loop {
        if r.pos + 5 > r.end {
            return None;
        }
        let id = r.read(5) as u8;
        if id == 0 {
            break;
        }
        let smploffste = r.read_bit();
        if smploffste {
            r.read(12);
        }
        if r.read_bit() {
            r.read_var(11, 8);
        }
        if r.read_bit() {
            r.read_var(2, 8);
        }
        if r.read_bit() {
            r.read(8);
        }
        if !r.read_bit() {
            let mut fa = false;
            if !smploffste {
                fa = r.read_bit();
                if fa {
                    r.read(2);
                }
            }
            if smploffste || fa {
                r.read(7);
            }
        }
        let size = r.read_var(8, 8) as usize;
        r.pos += size * 8;
        if r.pos > r.end {
            return None;
        }
    }
    let lenmap = [0u32, 8, 32, 128];
    let prim_bits = lenmap[r.read(2) as usize];
    let sec_bits = lenmap[r.read(2) as usize];
    let protection_start = r.pos;
    let container_bits = 32 + emdf_len * 8;
    if container_bits > aligned.len() * 8
        || protection_start + prim_bits as usize + sec_bits as usize > container_bits
    {
        return None;
    }
    Some(EmdfFieldOffsets {
        key_id_bit,
        protection_start,
        prim_bits,
        sec_bits,
    })
}
