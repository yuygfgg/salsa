//! Minimal IAMF OBU scanner used only to find tile boundaries for BaseParse.
//!
//! The codec parser owns all syntax interpretation. This scanner deliberately
//! understands nothing beyond the IAMF OBU header so that the GStreamer adapter
//! can hand complete 100 ms tiles to `GstBaseParse`.

const OBU_TEMPORAL_DELIMITER: u8 = 4;
const TILE_PACKETS: usize = 5;
const OBU_AUDIO_FRAME: u8 = 5;
const OBU_AUDIO_FRAME_ID0: u8 = 6;
const OBU_AUDIO_FRAME_ID17: u8 = 23;

struct Obu {
    obu_type: u8,
    next: usize,
}

fn is_audio_frame_type(obu_type: u8) -> bool {
    (OBU_AUDIO_FRAME_ID0..=OBU_AUDIO_FRAME_ID17).contains(&obu_type) || obu_type == OBU_AUDIO_FRAME
}

fn read_uleb(data: &[u8], position: &mut usize) -> Option<u64> {
    let start = *position;
    let mut value = 0u64;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = *data.get(*position)?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
    *position = start;
    None
}

/// Parse the OBU starting at `offset`, returning its type and end offset.
fn obu_at(data: &[u8], offset: usize) -> Option<Obu> {
    let first = *data.get(offset)?;
    let obu_type = first >> 3;
    let type_specific = first & 0x02 != 0;
    let extension_flag = first & 0x01 != 0;

    let mut position = offset + 1;
    let obu_size = read_uleb(data, &mut position)?;
    let size_start = position;

    if type_specific && is_audio_frame_type(obu_type) {
        read_uleb(data, &mut position)?;
        read_uleb(data, &mut position)?;
    }
    if extension_flag {
        let extension_size = read_uleb(data, &mut position)?;
        let end = position.checked_add(usize::try_from(extension_size).ok()?)?;
        if end > data.len() {
            return None;
        }
        position = end;
    }

    let fields_len = position - size_start;
    let payload_size = usize::try_from(obu_size.checked_sub(fields_len as u64)?).ok()?;
    let next = position.checked_add(payload_size)?;
    if next > data.len() {
        return None;
    }
    Some(Obu { obu_type, next })
}

/// Return the byte length of the complete tile beginning at `data[0]`.
///
/// Every 20 ms packet group starts with a temporal delimiter. A normal tile
/// contains five such groups, so the sixth delimiter starts the next tile.
/// The final tile can be shorter and is returned by the codec parser's
/// `flush`, not by this scanner.
pub fn tile_bytes(data: &[u8]) -> Option<usize> {
    let mut position = 0usize;
    let mut delimiters = 0usize;
    while position < data.len() {
        let current = obu_at(data, position)?;
        if current.obu_type == OBU_TEMPORAL_DELIMITER {
            if delimiters == TILE_PACKETS {
                return Some(position);
            }
            delimiters += 1;
        }
        position = current.next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![obu_type << 3];
        let mut size = payload.len() as u64;
        loop {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if size == 0 {
                break;
            }
        }
        out.extend_from_slice(payload);
        out
    }

    fn packet(data: &mut Vec<u8>) {
        data.extend(obu(OBU_TEMPORAL_DELIMITER, &[]));
        data.extend(obu(OBU_AUDIO_FRAME_ID0, b"front"));
        data.extend(obu(OBU_AUDIO_FRAME_ID0 + 1, b"surround"));
        data.extend(obu(OBU_AUDIO_FRAME_ID0 + 2, b"height"));
        data.extend(obu(OBU_AUDIO_FRAME_ID0 + 3, b"center"));
        data.extend(obu(OBU_AUDIO_FRAME_ID0 + 4, b"lfe"));
    }

    #[test]
    fn finds_the_second_tile_marker() {
        let mut data = Vec::new();
        data.extend(obu(31, b"header"));
        for _ in 0..TILE_PACKETS {
            packet(&mut data);
        }
        let first_tile = data.len();
        for _ in 0..TILE_PACKETS {
            packet(&mut data);
        }
        assert_eq!(tile_bytes(&data), Some(first_tile));
    }

    #[test]
    fn needs_more_until_the_next_marker_arrives() {
        let mut data = Vec::new();
        for _ in 0..TILE_PACKETS {
            packet(&mut data);
        }
        assert_eq!(tile_bytes(&data), None);
    }

    #[test]
    fn waits_for_a_truncated_obu() {
        let mut data = Vec::new();
        for _ in 0..TILE_PACKETS {
            packet(&mut data);
        }
        data.push((OBU_TEMPORAL_DELIMITER << 3) | 0x80);
        assert_eq!(tile_bytes(&data), None);
    }

    #[test]
    fn frames_real_writer_output() {
        use objectaudio_ir::ObjectState;
        use salsa_spatial::{
            CorePacket, EnhancementTile, IamfWriter, SalsaSpatialFrame, SalsaSpatialStreamConfig,
        };

        let config = SalsaSpatialStreamConfig {
            object_count: 1,
            initial_objects: vec![ObjectState::new(1, [0.0, 1.0, 0.0])],
            ..Default::default()
        };
        let packet = CorePacket {
            front: vec![0x01],
            surround: vec![0x02],
            height: vec![0x03],
            center: vec![0x04],
            lfe: vec![0x05],
        };
        let frame = SalsaSpatialFrame {
            sample_rate: 48_000,
            start_sample: 0,
            frames: 4_800,
            packets: vec![packet; 5],
            enhancement: EnhancementTile {
                start_sample: 0,
                frames: 4_800,
                bytes: Vec::new(),
                snapshot: false,
            },
            objects: config.initial_objects.clone(),
            events: Vec::new(),
            snapshot: false,
            parameters: None,
        };
        let mut writer = IamfWriter::new(&config).unwrap();
        let first = writer.write_frame(&frame).unwrap();
        let second = writer.write_frame(&frame).unwrap();
        let mut stream = first.clone();
        stream.extend_from_slice(&second);
        assert_eq!(tile_bytes(&stream), Some(first.len()));
    }
}
