//! E-AC-3 syncframe extraction from a burst payload.

const EAC3_SYNCWORD: [u8; 2] = [0x0B, 0x77];
const EAC3_MIN_FRAME_BYTES: usize = 8;

/// One complete E-AC-3 syncframe recovered from a carrier burst.
///
/// Dependent substreams are separate syncframes and are returned in stream
/// order; this type never merges or reinterprets them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eac3AccessUnit {
    /// Session generation that produced the frame.
    pub generation: u64,
    /// Absolute carrier byte offset of the syncframe payload.
    pub carrier_offset: u64,
    /// Complete E-AC-3 syncframe, including the `0B77` sync word.
    pub bytes: Vec<u8>,
}

/// Cumulative E-AC-3 extraction counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Eac3Stats {
    pub frames: u64,
    pub corrupt_frames: u64,
    pub resynced_bytes: u64,
}

/// Streaming E-AC-3 frame splitter.
///
/// The deprecated compressed-burst description carried one frame per burst; the
/// current description carries a complete burst-sized payload. This splitter
/// accepts either and keeps an incomplete tail for the next burst.
#[derive(Debug, Default)]
pub struct Eac3Depacketizer {
    pending: Vec<u8>,
    pending_offset: u64,
    stats: Eac3Stats,
}

impl Eac3Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn stats(&self) -> Eac3Stats {
        self.stats
    }

    /// Drop partial input.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.pending_offset = 0;
    }

    /// Feed one burst payload and return every complete syncframe.
    pub fn push(
        &mut self,
        payload: &[u8],
        payload_carrier_offset: u64,
        generation: u64,
    ) -> Vec<Eac3AccessUnit> {
        if self.pending.is_empty() {
            self.pending_offset = payload_carrier_offset;
        }
        self.pending.extend_from_slice(payload);

        let mut frames = Vec::new();
        let mut cursor = 0usize;

        loop {
            if self.pending.len() - cursor < EAC3_MIN_FRAME_BYTES {
                break;
            }

            if self.pending[cursor..cursor + 2] != EAC3_SYNCWORD {
                let search_start = cursor + 2;
                match find_syncword(&self.pending[search_start..]) {
                    Some(relative) => {
                        let skip = 2 + relative;
                        self.stats.corrupt_frames += 1;
                        self.stats.resynced_bytes += skip as u64;
                        cursor += skip;
                        continue;
                    }
                    None => {
                        let keep = self.pending.len().saturating_sub(cursor).min(1);
                        cursor = self.pending.len() - keep;
                        break;
                    }
                }
            }

            let size = frame_bytes(&self.pending[cursor..]);
            let Some(size) = size else {
                self.stats.corrupt_frames += 1;
                self.stats.resynced_bytes += 2;
                cursor += 2;
                continue;
            };

            if self.pending.len() - cursor < size {
                break;
            }

            let bytes = self.pending[cursor..cursor + size].to_vec();
            frames.push(Eac3AccessUnit {
                generation,
                carrier_offset: self.pending_offset + cursor as u64,
                bytes,
            });
            self.stats.frames += 1;
            cursor += size;
        }

        if cursor > 0 {
            self.pending.drain(..cursor);
            self.pending_offset += cursor as u64;
        }
        if self.pending.is_empty() {
            self.pending_offset = 0;
        }
        frames
    }
}

fn find_syncword(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(EAC3_SYNCWORD.len())
        .position(|window| window == EAC3_SYNCWORD)
}

/// E-AC-3 frame size in bytes: `frmsiz` counts 16-bit words after the sync word.
fn frame_bytes(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 || bytes[..2] != EAC3_SYNCWORD {
        return None;
    }
    let frame_size_words = (((bytes[2] & 0x07) as usize) << 8) | bytes[3] as usize;
    let size = (frame_size_words + 1) * 2;
    (size >= EAC3_MIN_FRAME_BYTES).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_frame(size: usize, fill: u8) -> Vec<u8> {
        assert!(size.is_multiple_of(2) && size >= EAC3_MIN_FRAME_BYTES);
        let words = size / 2 - 1;
        let mut out = vec![0u8; size];
        out[..2].copy_from_slice(&EAC3_SYNCWORD);
        out[2] = ((words >> 8) & 0x07) as u8;
        out[3] = (words & 0xFF) as u8;
        out[4..].fill(fill);
        out
    }

    #[test]
    fn splits_frames_and_preserves_dependent_substreams() {
        let first = fake_frame(32, 0x11);
        let dependent = fake_frame(24, 0x22);
        let mut payload = first.clone();
        payload.extend_from_slice(&dependent);
        let mut depacketizer = Eac3Depacketizer::new();
        let frames = depacketizer.push(&payload, 100, 7);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].bytes, first);
        assert_eq!(frames[1].bytes, dependent);
        assert_eq!(frames[0].carrier_offset, 100);
        assert_eq!(frames[1].carrier_offset, 132);
        assert_eq!(frames[0].generation, 7);
    }

    #[test]
    fn keeps_an_incomplete_frame_for_the_next_burst() {
        let frame = fake_frame(64, 0x33);
        let mut depacketizer = Eac3Depacketizer::new();
        assert!(depacketizer.push(&frame[..20], 0, 0).is_empty());
        let frames = depacketizer.push(&frame[20..], 20, 0);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes, frame);
    }

    #[test]
    fn resynchronises_on_the_next_sync_word() {
        let frame = fake_frame(16, 0x44);
        let mut payload = vec![0xAA, 0xBB, 0xCC];
        payload.extend_from_slice(&frame);
        let mut depacketizer = Eac3Depacketizer::new();
        let frames = depacketizer.push(&payload, 0, 0);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes, frame);
        assert_eq!(depacketizer.stats().corrupt_frames, 1);
    }
}
