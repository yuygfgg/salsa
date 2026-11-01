//! Minimal Core Audio Format (CAF) reader for uncompressed DAMF essence.
//!
//! DAMF `.atmos.audio` files are CAF `lpcm` streams, normally 24-bit
//! big-endian integer samples. This reader deliberately supports only the
//! uncompressed layouts needed by DAMF and returns interleaved `f32` samples in
//! the file's channel order.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::{DamfError, Result};

const CAFF_MAGIC: &[u8; 4] = b"caff";
const DESC_CHUNK: &[u8; 4] = b"desc";
const DATA_CHUNK: &[u8; 4] = b"data";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Int,
    Float,
}

/// Parsed CAF `desc` and `data` chunk metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CafInfo {
    pub sample_rate: u32,
    pub channels: usize,
    pub bits: u32,
    pub bytes_per_sample: usize,
    pub bytes_per_frame: usize,
    pub sample_format: SampleFormat,
    pub big_endian: bool,
    pub data_offset: u64,
    pub data_bytes: Option<u64>,
}

impl CafInfo {
    pub fn total_frames(&self) -> Option<u64> {
        self.data_bytes
            .map(|bytes| bytes / self.bytes_per_frame as u64)
    }
}

/// Sequential reader for the CAF data chunk.
#[derive(Debug)]
pub struct CafReader {
    file: File,
    path: PathBuf,
    info: CafInfo,
    consumed_bytes: u64,
}

impl CafReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path).map_err(|source| DamfError::Io {
            path: path.clone(),
            source,
        })?;
        let file_len = file
            .metadata()
            .map_err(|source| DamfError::Io {
                path: path.clone(),
                source,
            })?
            .len();

        let mut magic = [0u8; 4];
        read_exact(&mut file, &mut magic, &path)?;
        if &magic != CAFF_MAGIC {
            return Err(DamfError::UnsupportedCaf(
                "missing caff sync word".to_string(),
            ));
        }
        // version and flags
        let mut header = [0u8; 4];
        read_exact(&mut file, &mut header, &path)?;

        let mut desc: Option<RawDesc> = None;
        loop {
            let mut chunk_type = [0u8; 4];
            read_exact(&mut file, &mut chunk_type, &path)?;
            let mut size_bytes = [0u8; 8];
            read_exact(&mut file, &mut size_bytes, &path)?;
            let chunk_size = i64::from_be_bytes(size_bytes);

            match &chunk_type {
                DESC_CHUNK => {
                    if chunk_size < 32 {
                        return Err(DamfError::UnsupportedCaf(
                            "CAF desc chunk is shorter than 32 bytes".to_string(),
                        ));
                    }
                    desc = Some(read_desc(&mut file, &path)?);
                    let extra = chunk_size - 32;
                    if extra > 0 {
                        file.seek(SeekFrom::Current(extra))
                            .map_err(|source| DamfError::Io {
                                path: path.clone(),
                                source,
                            })?;
                    }
                }
                DATA_CHUNK => {
                    let Some(desc) = desc else {
                        return Err(DamfError::UnsupportedCaf(
                            "CAF data chunk appears before desc".to_string(),
                        ));
                    };
                    if (0..4).contains(&chunk_size) {
                        return Err(DamfError::UnsupportedCaf(
                            "CAF data chunk is shorter than the edit count".to_string(),
                        ));
                    }
                    // The first four bytes of a CAF data chunk are an edit count.
                    let mut edit_count = [0u8; 4];
                    read_exact(&mut file, &mut edit_count, &path)?;
                    let data_offset = file.stream_position().map_err(|source| DamfError::Io {
                        path: path.clone(),
                        source,
                    })?;
                    let data_bytes = if chunk_size < 0 {
                        file_len.saturating_sub(data_offset)
                    } else {
                        (chunk_size as u64).saturating_sub(4)
                    };
                    let bytes_per_sample = (desc.bits / 8) as usize;
                    let bytes_per_frame = (desc.channels as usize)
                        .checked_mul(bytes_per_sample)
                        .ok_or_else(|| {
                            DamfError::UnsupportedCaf(
                                "CAF channel/sample size overflow".to_string(),
                            )
                        })?;
                    if bytes_per_frame == 0 {
                        return Err(DamfError::UnsupportedCaf(
                            "CAF frame size is zero".to_string(),
                        ));
                    }
                    let info = CafInfo {
                        sample_rate: desc.sample_rate,
                        channels: desc.channels as usize,
                        bits: desc.bits,
                        bytes_per_sample,
                        bytes_per_frame,
                        sample_format: desc.sample_format,
                        big_endian: desc.big_endian,
                        data_offset,
                        data_bytes: Some(data_bytes),
                    };
                    file.seek(SeekFrom::Start(data_offset))
                        .map_err(|source| DamfError::Io {
                            path: path.clone(),
                            source,
                        })?;
                    return Ok(Self {
                        file,
                        path,
                        info,
                        consumed_bytes: 0,
                    });
                }
                _ => {
                    if chunk_size < 0 {
                        return Err(DamfError::UnsupportedCaf(format!(
                            "unknown CAF chunk {:?} with negative size",
                            String::from_utf8_lossy(&chunk_type)
                        )));
                    }
                    file.seek(SeekFrom::Current(chunk_size))
                        .map_err(|source| DamfError::Io {
                            path: path.clone(),
                            source,
                        })?;
                }
            }
        }
    }

    pub fn info(&self) -> &CafInfo {
        &self.info
    }

    pub fn total_frames(&self) -> Option<u64> {
        self.info.total_frames()
    }

    /// Move the read cursor to `frame` (clamped to the end of the essence).
    pub fn seek_to_frame(&mut self, frame: u64) -> Result<()> {
        let frame = self
            .info
            .total_frames()
            .map_or(frame, |total| frame.min(total));
        let offset = frame
            .checked_mul(self.info.bytes_per_frame as u64)
            .ok_or_else(|| DamfError::UnsupportedCaf("CAF seek offset overflow".to_string()))?;
        let position =
            self.info.data_offset.checked_add(offset).ok_or_else(|| {
                DamfError::UnsupportedCaf("CAF seek position overflow".to_string())
            })?;
        self.file
            .seek(SeekFrom::Start(position))
            .map_err(|source| DamfError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.consumed_bytes = offset;
        Ok(())
    }

    /// Read up to `frames` interleaved samples in the CAF channel order.
    pub fn read_frames(&mut self, frames: usize) -> Result<Vec<f32>> {
        let max_frames = self
            .info
            .data_bytes
            .map(|bytes| {
                bytes.saturating_sub(self.consumed_bytes) / self.info.bytes_per_frame as u64
            })
            .unwrap_or(u64::MAX);
        let frames = (frames as u64).min(max_frames) as usize;
        if frames == 0 {
            return Ok(Vec::new());
        }
        let bytes = frames * self.info.bytes_per_frame;
        let mut raw = vec![0u8; bytes];
        self.file
            .read_exact(&mut raw)
            .map_err(|source| DamfError::Io {
                path: self.path.clone(),
                source,
            })?;
        self.consumed_bytes += bytes as u64;
        decode_interleaved(&raw, &self.info)
    }
}

#[derive(Debug)]
struct RawDesc {
    sample_rate: u32,
    channels: u32,
    bits: u32,
    sample_format: SampleFormat,
    big_endian: bool,
}

fn read_desc(file: &mut File, path: &Path) -> Result<RawDesc> {
    let mut buf = [0u8; 32];
    read_exact(file, &mut buf, path)?;

    let sample_rate = f64::from_be_bytes(buf[0..8].try_into().unwrap());
    if !sample_rate.is_finite() || sample_rate <= 0.0 || sample_rate > u32::MAX as f64 {
        return Err(DamfError::UnsupportedCaf(format!(
            "invalid CAF sample rate {sample_rate}"
        )));
    }
    if &buf[8..12] != b"lpcm" {
        return Err(DamfError::UnsupportedCaf(format!(
            "unsupported CAF format {:?}",
            String::from_utf8_lossy(&buf[8..12])
        )));
    }
    let flags = u32::from_be_bytes(buf[12..16].try_into().unwrap());
    let bytes_per_packet = u32::from_be_bytes(buf[16..20].try_into().unwrap());
    let frames_per_packet = u32::from_be_bytes(buf[20..24].try_into().unwrap());
    let channels = u32::from_be_bytes(buf[24..28].try_into().unwrap());
    let bits = u32::from_be_bytes(buf[28..32].try_into().unwrap());

    if channels == 0 {
        return Err(DamfError::UnsupportedCaf(
            "CAF has zero channels".to_string(),
        ));
    }
    if frames_per_packet != 1 {
        return Err(DamfError::UnsupportedCaf(format!(
            "CAF frames_per_packet={frames_per_packet}; only packed PCM is supported"
        )));
    }
    let expected = channels
        .checked_mul(bits / 8)
        .ok_or_else(|| DamfError::UnsupportedCaf("CAF packet size overflow".to_string()))?;
    if bytes_per_packet != 0 && bytes_per_packet != expected {
        return Err(DamfError::UnsupportedCaf(format!(
            "CAF bytes_per_packet={bytes_per_packet}, expected {expected}"
        )));
    }
    let sample_format = if flags & 0x1 != 0 {
        SampleFormat::Float
    } else {
        SampleFormat::Int
    };
    if bits % 8 != 0 || bits == 0 {
        return Err(DamfError::UnsupportedCaf(format!(
            "unsupported CAF bit depth {bits}"
        )));
    }
    match (sample_format, bits) {
        (SampleFormat::Int, 16 | 24 | 32) => {}
        (SampleFormat::Float, 32) => {}
        _ => {
            return Err(DamfError::UnsupportedCaf(format!(
                "unsupported CAF sample format {sample_format:?}/{bits}-bit"
            )));
        }
    }

    Ok(RawDesc {
        sample_rate: sample_rate.round() as u32,
        channels,
        bits,
        sample_format,
        // CAF bit 1 means little-endian; clear means big-endian.
        big_endian: flags & 0x2 == 0,
    })
}

fn decode_interleaved(raw: &[u8], info: &CafInfo) -> Result<Vec<f32>> {
    let sample_bytes = info.bytes_per_sample;
    let count = raw.len() / sample_bytes;
    let mut out = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(sample_bytes) {
        out.push(decode_sample(chunk, info));
    }
    Ok(out)
}

fn decode_sample(bytes: &[u8], info: &CafInfo) -> f32 {
    match info.sample_format {
        SampleFormat::Float => {
            let bits = if info.big_endian {
                u32::from_be_bytes(bytes.try_into().expect("4-byte float sample"))
            } else {
                u32::from_le_bytes(bytes.try_into().expect("4-byte float sample"))
            };
            f32::from_bits(bits)
        }
        SampleFormat::Int => match info.bits {
            16 => {
                let value = if info.big_endian {
                    i16::from_be_bytes(bytes.try_into().expect("2-byte sample"))
                } else {
                    i16::from_le_bytes(bytes.try_into().expect("2-byte sample"))
                };
                f32::from(value) / 32_768.0
            }
            24 => {
                let raw = if info.big_endian {
                    (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2])
                } else {
                    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
                };
                let signed = if raw & 0x0080_0000 != 0 {
                    (raw | 0xFF00_0000) as i32
                } else {
                    raw as i32
                };
                signed as f32 / 8_388_608.0
            }
            32 => {
                let value = if info.big_endian {
                    i32::from_be_bytes(bytes.try_into().expect("4-byte sample"))
                } else {
                    i32::from_le_bytes(bytes.try_into().expect("4-byte sample"))
                };
                value as f32 / 2_147_483_648.0
            }
            _ => 0.0,
        },
    }
}

fn read_exact(file: &mut File, buf: &mut [u8], path: &Path) -> Result<()> {
    file.read_exact(buf).map_err(|source| DamfError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("salsa-caf-{}-{nonce}-{name}", std::process::id()))
    }

    fn write_caf(path: &Path, sample_rate: u32, channels: u32, bits: u32, samples: &[f32]) {
        assert_eq!(bits, 24);
        let bytes_per_sample = 3u32;
        let data_len = (samples.len() * bytes_per_sample as usize) as u64;
        let mut file = File::create(path).unwrap();
        file.write_all(b"caff").unwrap();
        file.write_all(&1u16.to_be_bytes()).unwrap();
        file.write_all(&0u16.to_be_bytes()).unwrap();

        file.write_all(b"desc").unwrap();
        file.write_all(&32u64.to_be_bytes()).unwrap();
        file.write_all(&f64::from(sample_rate).to_be_bytes())
            .unwrap();
        file.write_all(b"lpcm").unwrap();
        file.write_all(&0u32.to_be_bytes()).unwrap();
        file.write_all(&(channels * bytes_per_sample).to_be_bytes())
            .unwrap();
        file.write_all(&1u32.to_be_bytes()).unwrap();
        file.write_all(&channels.to_be_bytes()).unwrap();
        file.write_all(&bits.to_be_bytes()).unwrap();

        file.write_all(b"data").unwrap();
        file.write_all(&(data_len + 4).to_be_bytes()).unwrap();
        file.write_all(&0u32.to_be_bytes()).unwrap();
        for sample in samples {
            let value = (sample * 8_388_608.0).round() as i32;
            file.write_all(&value.to_be_bytes()[1..4]).unwrap();
        }
    }

    #[test]
    fn seeks_to_a_frame() {
        let path = unique_path("seek.caf");
        let samples = [0.0f32, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
        write_caf(&path, 48_000, 1, 24, &samples);
        let mut reader = CafReader::open(&path).unwrap();
        reader.seek_to_frame(5).unwrap();
        let decoded = reader.read_frames(4).unwrap();
        assert_eq!(decoded.len(), 4);
        assert!((decoded[0] - 0.5).abs() < 1e-6);
        assert!((decoded[3] - 0.8).abs() < 1e-6);
        // Seeking past the end clamps to EOF.
        reader.seek_to_frame(100).unwrap();
        assert!(reader.read_frames(1).unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_24_bit_big_endian_pcm() {
        let path = unique_path("read.caf");
        let samples = [0.0f32, 0.5, -0.5, 0.25, -0.25, 0.75];
        write_caf(&path, 48_000, 2, 24, &samples);
        let mut reader = CafReader::open(&path).unwrap();
        assert_eq!(reader.info().sample_rate, 48_000);
        assert_eq!(reader.info().channels, 2);
        assert_eq!(reader.total_frames(), Some(3));
        let decoded = reader.read_frames(3).unwrap();
        assert_eq!(decoded.len(), samples.len());
        for (actual, expected) in decoded.iter().zip(samples) {
            assert!((actual - expected).abs() < 1.0 / 8_388_608.0);
        }
        assert!(reader.read_frames(1).unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
