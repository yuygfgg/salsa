// SPDX-License-Identifier: MPL-2.0
//! 5.1 bed downmix for the Atmos encoder.
//!
//! Each dynamic object is panned by its horizontal azimuth onto the standard
//! 5.1 speaker ring. The LFE bed passes through. Per-object gains are linearly
//! interpolated between metadata position keyframes so moving objects track
//! smoothly.
//!
//! This module also owns the encoder panning laws (`hard71` default, `hard51`
//! and the legacy `vbap`).

use std::collections::{HashMap, VecDeque};

use anyhow::{Context, Result, ensure};
use objectaudio_damf::{CafReader, Manifest, Metadata};

use crate::phase_shift;

/// 5.1 speaker ring as (azimuth degrees, index into the returned `[L,R,C,Ls,Rs]` gain array).
/// Azimuth is measured from front (+y) toward right (+x): 0=C, +30=R, +110=Rs, −30=L, −110=Ls.
const RING: [(f64, usize); 5] = [
    (-110.0, 3), // Ls
    (-30.0, 0),  // L
    (0.0, 2),    // C
    (30.0, 1),   // R
    (110.0, 4),  // Rs
];

/// Downmix panning law.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanMode {
    Vbap,
    Hard51,
    Hard71,
}

const FRONT_HEIGHT_TRIM: f32 = 0.707_945_8; // -3.0 dB
const SURROUND_HEIGHT_TRIM: f32 = 0.944_060_9; // -0.5 dB

/// 7.1 speaker ring as (azimuth degrees, index into the 7-channel gain array
/// `[L,R,C,Ls,Rs,Lrs,Rrs]`). `Lrs`/`Rrs` are folded into the 5.1 `Ls`/`Rs` output.
const RING71: [(f64, usize); 7] = [
    (-150.0, 5), // Lrs
    (-90.0, 3),  // Ls
    (-30.0, 0),  // L
    (0.0, 2),    // C
    (30.0, 1),   // R
    (90.0, 4),   // Rs
    (150.0, 6),  // Rrs
];

/// Per-speaker gains measured from the 16ch.mlp 7.1 presentation for static objects:
/// fronts 0 dB, side surrounds -1.24 dB, rear surrounds -1.5 dB.
const GAIN71: [f32; 7] = [1.0, 1.0, 1.0, 0.867, 0.867, 0.842, 0.842];

fn nearest_speaker(az: f64) -> usize {
    let mut best = 2;
    let mut best_d = f64::INFINITY;
    for (a, idx) in RING {
        let mut d = (az - a).abs() % 360.0;
        if d > 180.0 {
            d = 360.0 - d;
        }
        if d < best_d {
            best_d = d;
            best = idx;
        }
    }
    best
}

/// Legacy 5.1 hard pan: one core speaker per object, except the rear-centre tie (Ls+Rs).
pub(crate) fn pan_hard51(x: f64, y: f64, z: f64) -> [f32; 5] {
    let mut g = [0.0f32; 5];
    if x.abs() < 1e-9 && y <= 0.0 {
        g[3] = std::f32::consts::FRAC_1_SQRT_2;
        g[4] = std::f32::consts::FRAC_1_SQRT_2;
    } else {
        let az = x.atan2(y).to_degrees();
        g[nearest_speaker(az)] = 1.0;
    }
    if z > 0.0 {
        let trim = if g[0] != 0.0 || g[1] != 0.0 || g[2] != 0.0 {
            FRONT_HEIGHT_TRIM
        } else {
            SURROUND_HEIGHT_TRIM
        };
        for v in &mut g {
            *v *= trim;
        }
    }
    g
}

fn nearest_speaker71(az: f64) -> usize {
    let mut best = 2;
    let mut best_d = f64::INFINITY;
    for (a, idx) in RING71 {
        let mut d = (az - a).abs() % 360.0;
        if d > 180.0 {
            d = 360.0 - d;
        }
        if d < best_d {
            best_d = d;
            best = idx;
        }
    }
    best
}

/// 7.1 hard pan with the measured 6ch presentation speaker gains, folded to `[L,R,C,Ls,Rs]`.
pub(crate) fn pan_hard71(x: f64, y: f64, z: f64) -> [f32; 5] {
    let mut g = [0.0f32; 7];
    if x.abs() < 1e-9 && y <= 0.0 {
        g[5] = std::f32::consts::FRAC_1_SQRT_2;
        g[6] = std::f32::consts::FRAC_1_SQRT_2;
    } else {
        let az = x.atan2(y).to_degrees();
        g[nearest_speaker71(az)] = 1.0;
    }
    for (v, trim) in g.iter_mut().zip(GAIN71) {
        *v *= trim;
    }
    if z > 0.0 {
        for (i, v) in g.iter_mut().enumerate() {
            if *v != 0.0 {
                *v *= if i < 3 {
                    FRONT_HEIGHT_TRIM
                } else {
                    SURROUND_HEIGHT_TRIM
                };
            }
        }
    }
    [g[0], g[1], g[2], g[3] + g[5], g[4] + g[6]]
}

/// Constant-power pairwise pan of an object at room position (x,y) to `[L,R,C,Ls,Rs]` gains.
pub(crate) fn pan_vbap(x: f64, y: f64) -> [f32; 5] {
    let mut g = [0.0f32; 5];
    let az = x.atan2(y).to_degrees();
    let n = RING.len();
    for i in 0..n {
        let (a0, idx0) = RING[i];
        let (a1_raw, idx1) = RING[(i + 1) % n];
        let wrap = i + 1 == n;
        let a1 = if wrap { a1_raw + 360.0 } else { a1_raw };
        let azc = if wrap && az < a0 { az + 360.0 } else { az };
        if azc >= a0 && azc <= a1 {
            let frac = (azc - a0) / (a1 - a0);
            let ang = frac * std::f64::consts::FRAC_PI_2;
            g[idx0] = ang.cos() as f32;
            g[idx1] = ang.sin() as f32;
            return g;
        }
    }
    g[2] = 1.0;
    g
}

/// One object's panning timeline: essence channel + (samplePos, gains) keyframes.
#[derive(Debug)]
pub(crate) struct ObjEnv {
    pub(crate) ess_ch: usize,
    pub(crate) keys: Vec<(u64, [f32; 5])>,
    pub(crate) cursor: usize,
}

impl ObjEnv {
    /// Gains at absolute sample index `t`, advancing the cursor (t is monotonically increasing).
    pub(crate) fn gains_at(&mut self, t: u64) -> [f32; 5] {
        if self.keys.is_empty() {
            return [0.0; 5];
        }
        while self.cursor + 1 < self.keys.len() && self.keys[self.cursor + 1].0 <= t {
            self.cursor += 1;
        }
        let (p0, g0) = self.keys[self.cursor];
        if self.cursor + 1 < self.keys.len() {
            let (p1, g1) = self.keys[self.cursor + 1];
            let frac = if p1 > p0 && t > p0 {
                ((t - p0) as f32) / ((p1 - p0) as f32)
            } else {
                0.0
            };
            let mut g = [0.0f32; 5];
            for k in 0..5 {
                g[k] = g0[k] + (g1[k] - g0[k]) * frac;
            }
            g
        } else {
            g0
        }
    }
}

/// Build each element's per-sample panning gain keyframes from the diff-encoded metadata events.
pub(crate) fn gain_keyframes(meta: &Metadata, mode: PanMode) -> HashMap<i64, Vec<(u64, [f32; 5])>> {
    let mut cur: HashMap<i64, ([f64; 3], bool)> = HashMap::new();
    let mut keys: HashMap<i64, Vec<(u64, [f32; 5])>> = HashMap::new();
    for e in &meta.events {
        let id = e.id;
        let st = cur.entry(id).or_insert(([0.0, 0.0, 0.0], true));
        if let Some(p) = e.position {
            st.0 = [f64::from(p[0]), f64::from(p[1]), f64::from(p[2])];
        }
        if let Some(a) = e.active {
            st.1 = a;
        }
        let g = if st.1 {
            match mode {
                PanMode::Vbap => pan_vbap(st.0[0], st.0[1]),
                PanMode::Hard51 => pan_hard51(st.0[0], st.0[1], st.0[2]),
                PanMode::Hard71 => pan_hard71(st.0[0], st.0[1], st.0[2]),
            }
        } else {
            [0.0; 5]
        };
        keys.entry(id).or_default().push((e.sample_pos, g));
    }
    keys
}

/// Summary returned by [`write_downmix`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct DownmixInfo {
    pub(crate) frames: u64,
    pub(crate) sample_rate: u32,
    pub(crate) object_count: usize,
}

/// Streaming downmix reader: yields interleaved `L R C LFE Ls Rs` f32 blocks.
pub(crate) struct DownmixReader {
    reader: CafReader,
    objects: Vec<ObjEnv>,
    lfe_ch: usize,
    channels: usize,
    total_frames: u64,
    t: u64,
    lin: f32,
    shift: Option<phase_shift::SurroundPhaseShift>,
    pending: VecDeque<[f32; 4]>,
    ls_in: Vec<f32>,
    rs_in: Vec<f32>,
    ls_out: Vec<f32>,
    rs_out: Vec<f32>,
    finished: bool,
    info: DownmixInfo,
}

impl DownmixReader {
    pub(crate) fn new(
        manifest: &Manifest,
        metadata: &Metadata,
        gain_db: f64,
        mode: PanMode,
        surround_phase_shift: bool,
    ) -> Result<Self> {
        let bed_count = manifest.beds.len();
        ensure!(
            bed_count == 1 && manifest.beds[0].name.eq_ignore_ascii_case("LFE"),
            "the encoder supports an LFE-only bed; found {bed_count} bed channel(s)"
        );
        ensure!(
            metadata.sample_rate == 48_000,
            "reac3e supports 48 kHz only (DAMF is {} Hz)",
            metadata.sample_rate
        );

        let mut id_to_ch: HashMap<i64, usize> = HashMap::new();
        let mut lfe_ch = None;
        for (ch, bed) in manifest.beds.iter().enumerate() {
            if bed.name.eq_ignore_ascii_case("LFE") {
                lfe_ch = Some(ch);
            }
            id_to_ch.insert(bed.id, ch);
        }
        for (index, object) in manifest.objects.iter().enumerate() {
            id_to_ch.insert(object.id, bed_count + index);
        }

        let mut keys = gain_keyframes(metadata, mode);
        let objects: Vec<ObjEnv> = manifest
            .objects
            .iter()
            .map(|object| ObjEnv {
                ess_ch: id_to_ch[&object.id],
                keys: keys.remove(&object.id).unwrap_or_default(),
                cursor: 0,
            })
            .collect();
        let object_count = objects.len();
        let reader = CafReader::open(&manifest.audio)
            .with_context(|| format!("opening {}", manifest.audio.display()))?;
        let info = reader.info();
        let channels = info.channels;
        ensure!(
            channels == bed_count + object_count,
            "CAF has {channels} channels, manifest has {}",
            bed_count + object_count
        );
        ensure!(
            info.sample_rate == 48_000,
            "reac3e supports 48 kHz only (CAF is {} Hz)",
            info.sample_rate
        );
        let total_frames = info
            .total_frames()
            .context("CAF essence does not declare a data size")?;
        let lin = 10f32.powf((gain_db / 20.0) as f32);

        Ok(Self {
            reader,
            objects,
            lfe_ch: lfe_ch.expect("LFE bed"),
            channels,
            total_frames,
            t: 0,
            lin,
            shift: surround_phase_shift.then(phase_shift::SurroundPhaseShift::forward),
            pending: VecDeque::new(),
            ls_in: Vec::new(),
            rs_in: Vec::new(),
            ls_out: Vec::new(),
            rs_out: Vec::new(),
            finished: false,
            info: DownmixInfo {
                frames: total_frames,
                sample_rate: 48_000,
                object_count,
            },
        })
    }

    pub(crate) fn info(&self) -> DownmixInfo {
        self.info
    }

    /// Next interleaved 6-channel block, or `None` at the end of the stream.
    pub(crate) fn next_block(&mut self) -> Result<Option<Vec<f32>>> {
        if self.t >= self.total_frames {
            if self.shift.is_none() || self.finished {
                return Ok(None);
            }
            self.finished = true;
            let sh = self.shift.as_mut().expect("shifted path");
            sh.finish();
            self.ls_out.clear();
            self.rs_out.clear();
            sh.drain(&mut self.ls_out, &mut self.rs_out);
            ensure!(
                self.ls_out.len() == self.pending.len(),
                "phase shift produced {} frames for {} pending",
                self.ls_out.len(),
                self.pending.len()
            );
            if self.pending.is_empty() {
                return Ok(None);
            }
            let mut out = Vec::with_capacity(self.pending.len() * 6);
            for (pending, (&ls, &rs)) in self
                .pending
                .iter()
                .zip(self.ls_out.iter().zip(&self.rs_out))
            {
                out.extend_from_slice(&[pending[0], pending[1], pending[2], pending[3], ls, rs]);
            }
            self.pending.clear();
            return Ok(Some(out));
        }

        let want = ((self.total_frames - self.t) as usize).min(8192);
        let pcm = self.reader.read_frames(want)?;
        if pcm.is_empty() {
            self.t = self.total_frames;
            return Ok(Some(Vec::new()));
        }
        let frames = pcm.len() / self.channels;
        self.ls_in.clear();
        self.rs_in.clear();
        let mut out: Vec<f32> = Vec::with_capacity(frames * 6);
        for f in 0..frames {
            let base = f * self.channels;
            let mut o = [0f32; 6];
            o[3] = pcm[base + self.lfe_ch];
            for obj in &mut self.objects {
                let sample = pcm[base + obj.ess_ch];
                if sample == 0.0 {
                    obj.gains_at(self.t);
                    continue;
                }
                let gains = obj.gains_at(self.t);
                o[0] += sample * gains[0];
                o[1] += sample * gains[1];
                o[2] += sample * gains[2];
                o[4] += sample * gains[3];
                o[5] += sample * gains[4];
            }
            for k in [0usize, 1, 2, 4, 5] {
                o[k] *= self.lin;
            }
            if self.shift.is_some() {
                self.pending.push_back([o[0], o[1], o[2], o[3]]);
                self.ls_in.push(o[4]);
                self.rs_in.push(o[5]);
            } else {
                out.extend_from_slice(&o);
            }
            self.t += 1;
        }
        if let Some(sh) = self.shift.as_mut() {
            sh.feed(&self.ls_in, &self.rs_in);
            sh.produce();
            self.ls_out.clear();
            self.rs_out.clear();
            sh.drain(&mut self.ls_out, &mut self.rs_out);
            let n = self.ls_out.len().min(self.pending.len());
            for i in 0..n {
                let pending = self.pending.pop_front().expect("pending frame");
                out.extend_from_slice(&[
                    pending[0],
                    pending[1],
                    pending[2],
                    pending[3],
                    self.ls_out[i],
                    self.rs_out[i],
                ]);
            }
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn hard51_pan_front_left_is_l_only() {
        let g = pan_hard51(-1.0, 1.0, 0.0);
        assert!(approx(g[0], 1.0));
        for k in [1, 2, 3, 4] {
            assert!(approx(g[k], 0.0));
        }
    }

    #[test]
    fn hard51_pan_rear_centre_ties_ls_rs() {
        let g = pan_hard51(0.0, 0.0, 0.0);
        assert!(approx(g[3], std::f32::consts::FRAC_1_SQRT_2));
        assert!(approx(g[4], std::f32::consts::FRAC_1_SQRT_2));
    }

    #[test]
    fn hard71_folds_side_and_rear_surrounds() {
        assert!(approx(pan_hard71(-1.0, 0.0, 0.0)[3], 0.867));
        assert!(approx(pan_hard71(-1.0, -1.0, 0.0)[3], 0.842));
        assert!(approx(pan_hard71(1.0, 0.0, 0.0)[4], 0.867));
        assert!(approx(pan_hard71(1.0, -1.0, 0.0)[4], 0.842));
    }

    #[test]
    fn gain_keyframes_follow_position_and_active() {
        let yaml = br#"sampleRate: 48000
events:
  - ID: 10
    samplePos: 0
    active: true
    pos: [-1, 1, 0]
  - ID: 10
    samplePos: 100
    pos: [1, 1, 0]
  - ID: 10
    samplePos: 200
    active: false
"#;
        let path = std::env::temp_dir().join(format!(
            "salsa-encoder-gain-{}-{}.metadata",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, yaml).unwrap();
        let meta = Metadata::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let keys = gain_keyframes(&meta, PanMode::Hard51);
        let track = keys.get(&10).unwrap();
        assert_eq!(track.len(), 3);
        assert!(approx(track[0].1[0], 1.0));
        assert!(approx(track[1].1[1], 1.0));
        assert!(approx(track[2].1[0], 0.0));
    }
}
