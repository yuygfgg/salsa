// SPDX-License-Identifier: MPL-2.0
//! Per-object position timelines sampled for OAMD (ramp semantics).

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use objectaudio_damf::{Manifest, Metadata};

#[derive(Debug, Clone, Copy)]
struct PosKey {
    sample: u64,
    target: [f32; 3],
    ramp: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PosTimeline {
    keys: Vec<PosKey>,
    /// Position at each key's sample time (the previous key's ramp may still be running).
    starts: Vec<[f32; 3]>,
}

impl PosTimeline {
    fn new(mut keys: Vec<PosKey>) -> Self {
        keys.sort_by_key(|key| key.sample);
        let mut starts = Vec::with_capacity(keys.len());
        for j in 0..keys.len() {
            if j == 0 {
                starts.push(keys[0].target);
            } else {
                let prev = keys[j - 1];
                let f = if prev.ramp == 0 {
                    1.0
                } else {
                    ((keys[j].sample.saturating_sub(prev.sample)) as f32 / prev.ramp as f32)
                        .clamp(0.0, 1.0)
                };
                let a = starts[j - 1];
                let b = prev.target;
                starts.push([
                    a[0] + (b[0] - a[0]) * f,
                    a[1] + (b[1] - a[1]) * f,
                    a[2] + (b[2] - a[2]) * f,
                ]);
            }
        }
        Self { keys, starts }
    }

    /// Effective position at absolute sample `t`.
    pub(crate) fn at(&self, t: u64) -> [f32; 3] {
        if self.keys.is_empty() {
            return [0.0; 3];
        }
        let idx = self.keys.partition_point(|key| key.sample <= t);
        if idx == 0 {
            return self.keys[0].target;
        }
        let j = idx - 1;
        let key = self.keys[j];
        let start = self.starts[j];
        if key.ramp == 0 {
            return key.target;
        }
        let f = ((t - key.sample) as f32 / key.ramp as f32).clamp(0.0, 1.0);
        [
            start[0] + (key.target[0] - start[0]) * f,
            start[1] + (key.target[1] - start[1]) * f,
            start[2] + (key.target[2] - start[2]) * f,
        ]
    }
}

/// Build per-object timelines from the diff-encoded metadata events.
pub(crate) fn build_timelines(
    manifest: &Manifest,
    metadata: &Metadata,
) -> Result<Vec<PosTimeline>> {
    let mut cur_pos: BTreeMap<i64, [f32; 3]> = BTreeMap::new();
    let mut cur_ramp: BTreeMap<i64, u64> = BTreeMap::new();
    let mut keys: BTreeMap<i64, Vec<PosKey>> = BTreeMap::new();
    for event in &metadata.events {
        ensure!(
            event.trim_bypass != Some(true),
            "DAMF element {} sets trimBypass=true, but the OAMD trim_element encoder only \
             emits b_disable_trim_per_obj=0",
            event.id
        );
        if let Some(position) = event.position {
            cur_pos.insert(event.id, position);
        }
        if let Some(ramp) = event.ramp_length {
            cur_ramp.insert(event.id, u64::from(ramp));
        }
        keys.entry(event.id).or_default().push(PosKey {
            sample: event.sample_pos,
            target: *cur_pos.get(&event.id).unwrap_or(&[0.0; 3]),
            ramp: *cur_ramp.get(&event.id).unwrap_or(&0),
        });
    }
    Ok(manifest
        .objects
        .iter()
        .map(|object| PosTimeline::new(keys.remove(&object.id).unwrap_or_default()))
        .collect())
}

/// Sample all object positions at `t`.
pub(crate) fn positions_at(timelines: &[PosTimeline], t: u64) -> Vec<[f32; 3]> {
    timelines.iter().map(|timeline| timeline.at(t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_before_first_key_and_ramps_after_it() {
        let timeline = PosTimeline::new(vec![
            PosKey {
                sample: 100,
                target: [0.0, 0.0, 0.0],
                ramp: 0,
            },
            PosKey {
                sample: 300,
                target: [1.0, 0.0, 0.0],
                ramp: 100,
            },
        ]);
        assert_eq!(timeline.at(0), [0.0, 0.0, 0.0]);
        assert_eq!(timeline.at(100), [0.0, 0.0, 0.0]);
        assert_eq!(timeline.at(300), [0.0, 0.0, 0.0]);
        let mid = timeline.at(350);
        assert!((mid[0] - 0.5).abs() < 1e-6);
        assert_eq!(timeline.at(400), [1.0, 0.0, 0.0]);
        assert_eq!(timeline.at(500), [1.0, 0.0, 0.0]);
    }
}
